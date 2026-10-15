use crate::{
    error::bounded_text, registry::Request, Error, ExecutionGrant, GrantManifest, Principal,
    Result, ToolCall, ToolRegistry, TraceEvent, TraceStore,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;

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

/// Stable, explicitly selected model identity captured for a saved-agent task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelTarget {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub scope_id: String,
}
impl AgentModelTarget {
    pub fn validate(&self) -> Result<()> {
        for (text, limit) in [
            (&self.provider, 32),
            (&self.endpoint, 4096),
            (&self.model, 512),
            (&self.scope_id, 256),
        ] {
            if text.is_empty() || text.len() > limit || text.chars().any(char::is_control) {
                return Err(Error::InvalidInput("saved agent model target".into()));
            }
        }
        Ok(())
    }
}

/// Captured user-authored persona data, never an execution grant or system
/// authority. Changes to the saved definition cannot change an existing task.
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
impl AgentPersona {
    pub fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.definition_id).is_err()
            || self.definition_revision == 0
            || self.definition_revision > i64::MAX as u64
            || self.name.trim().is_empty()
            || self.name.len() > 80
            || self.name.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput("saved agent identity".into()));
        }
        bounded_text(&self.system_prompt, 8192, "agent system prompt")?;
        bounded_text(&self.soul, 4096, "agent soul")?;
        if let Some(target) = &self.model_target {
            target.validate()?;
        }
        Ok(())
    }
}

/// Immutable-to-engine audit snapshot. Mutating a returned snapshot changes no
/// durable state; commands always reload and compare-and-swap the stored row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub revision: u64,
    pub grant: GrantManifest,
    pub input: String,
    pub state: TaskState,
    pub plan: Vec<String>,
    pub clarification_answers: Vec<String>,
    pub tool_calls: BTreeSet<String>,
    pub last_result: Option<Value>,
    #[serde(default)]
    pub model_steps: u32,
    #[serde(default)]
    pub model_bytes: u64,
    #[serde(default)]
    pub last_model_reply: Option<ModelReply>,
    #[serde(default)]
    pub last_model_rejection: Option<crate::ModelRejectionRecord>,
    #[serde(default)]
    pub agent_persona: Option<AgentPersona>,
    #[serde(default)]
    pub extensions: crate::CapturedInstructions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReply {
    pub content: String,
    pub tool_call: Option<ToolCall>,
    pub provenance: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryDecision {
    ContinueWithoutReplay,
    RetryUncertainCall,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolOutcome {
    Returned(Value),
    Failed(Error),
    NeedsApproval,
    NeedsClarification,
    Completed,
}

/// Synchronous orchestration core. The host serializes orchestration per task,
/// but the store also enforces revision CAS across processes and UI requests.
/// Approval/recovery methods are trusted-host APIs, never model tool names.
pub struct TaskEngine<S: TraceStore> {
    store: S,
    registry: ToolRegistry,
}
impl<S: TraceStore> TaskEngine<S> {
    pub fn new(store: S, registry: ToolRegistry) -> Self {
        Self { store, registry }
    }
    pub fn store(&self) -> &S {
        &self.store
    }
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }
    pub fn get(&self, id: &str, grant: &ExecutionGrant) -> Result<Task> {
        let task = self.store.load(id)?;
        grant.validate_task(&task.grant)?;
        Ok(task)
    }

    /// Trusted-host observation by authenticated owner. This does not create
    /// filesystem authority and remains usable if a temporary workspace vanished.
    pub fn get_owned(&self, id: &str, owner: &Principal) -> Result<Task> {
        let task = self.store.load(id)?;
        if task.grant.principal != owner.id() {
            return Err(Error::Denied("task owner mismatch".into()));
        }
        Ok(task)
    }

    pub fn create(&self, grant: &ExecutionGrant, input: impl Into<String>) -> Result<Task> {
        self.create_with_context(grant, input, None, crate::CapturedInstructions::default())
    }

    pub fn create_with_persona(
        &self,
        grant: &ExecutionGrant,
        input: impl Into<String>,
        persona: AgentPersona,
    ) -> Result<Task> {
        self.create_with_context(
            grant,
            input,
            Some(persona),
            crate::CapturedInstructions::default(),
        )
    }

    /// Host-resolved bounded persona and extension snapshots. Neither input can
    /// confer a capability or resource that is absent from `grant`.
    pub fn create_with_context(
        &self,
        grant: &ExecutionGrant,
        input: impl Into<String>,
        agent_persona: Option<AgentPersona>,
        extensions: crate::CapturedInstructions,
    ) -> Result<Task> {
        extensions.validate()?;
        if let Some(persona) = &agent_persona {
            persona.validate()?;
        }
        if let Some(target) = agent_persona.as_ref().and_then(|p| p.model_target.as_ref()) {
            if !grant.permits(crate::Capability::ModelInference)
                || !grant.permits_resource(crate::ResourceKind::ProviderBinding, &target.scope_id)
            {
                return Err(Error::Denied(
                    "captured model target is not explicitly granted".into(),
                ));
            }
        }
        let input = input.into();
        bounded_text(&input, 16 * 1024, "task input")?;
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            grant: grant.manifest(),
            input,
            state: TaskState::Pending,
            plan: vec![],
            clarification_answers: vec![],
            tool_calls: BTreeSet::new(),
            last_result: None,
            model_steps: 0,
            model_bytes: 0,
            last_model_reply: None,
            last_model_rejection: None,
            agent_persona,
            extensions,
        };
        self.store.commit(None, &task, &TraceEvent::Created)?;
        Ok(task)
    }

    /// Reserve a real provider request before the host sends any task content.
    /// Selection/credentials remain host-owned; persisted IDs are audit data.
    #[allow(clippy::too_many_arguments)] // Identity, authority and audited provider/context values remain explicit.
    pub fn begin_model_at(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        scope_id: &str,
        binding_id: &str,
        context: Value,
        provenance: Value,
    ) -> Result<Task> {
        let mut task = self.get_at(id, revision, grant)?;
        ensure_idle(&task)?;
        if !grant.permits(crate::Capability::ModelInference)
            || !grant.permits_resource(crate::ResourceKind::ProviderBinding, scope_id)
        {
            return Err(Error::Denied(
                "Selected model target is not explicitly granted".into(),
            ));
        }
        if task
            .agent_persona
            .as_ref()
            .and_then(|p| p.model_target.as_ref())
            .is_some_and(|target| target.scope_id != scope_id)
        {
            return Err(Error::Denied(
                "Model target differs from the captured agent task".into(),
            ));
        }
        if scope_id.is_empty()
            || binding_id.is_empty()
            || scope_id.len() > 256
            || binding_id.len() > 256
        {
            return Err(Error::InvalidInput("Model binding".into()));
        }
        let bytes = context.to_string().len();
        bounded_text(&context.to_string(), 64 * 1024, "model context")?;
        bounded_text(&provenance.to_string(), 4096, "model provenance")?;
        if task.model_steps >= 64
            || task.model_bytes + bytes as u64 + 20 * 1024 > 4 * 1024 * 1024
            || task.revision > 1022
        {
            return Err(Error::LimitExceeded("Model step or context budget".into()));
        }
        let step_id = uuid::Uuid::new_v4().to_string();
        task.model_steps += 1;
        task.model_bytes += bytes as u64;
        task.state = TaskState::ModelThinking {
            step_id: step_id.clone(),
            scope_id: scope_id.into(),
            binding_id: binding_id.into(),
        };
        self.save(
            task,
            TraceEvent::ModelStarted {
                step_id,
                scope_id: scope_id.into(),
                binding_id: binding_id.into(),
                context,
                provenance,
            },
        )
    }
    pub fn finish_model_at(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        step_id: &str,
        reply: ModelReply,
    ) -> Result<Task> {
        let mut task = self.get_at(id, revision, grant)?;
        if !matches!(&task.state,TaskState::ModelThinking{step_id:current,..} if current==step_id) {
            return Err(invalid("No matching model reservation"));
        }
        bounded_text(&reply.content, 16 * 1024, "model response")?;
        bounded_text(&reply.provenance.to_string(), 4096, "model provenance")?;
        if let Some(call) = &reply.tool_call {
            bounded_text(
                &call.arguments.to_string(),
                16 * 1024,
                "model tool arguments",
            )?;
            if call.name.is_empty()
                || call.name.len() > 128
                || call.id.is_empty()
                || call.id.len() > 128
            {
                return Err(Error::InvalidInput("Model tool call identity".into()));
            }
        }
        let bytes = serde_json::to_string(&reply)
            .map_err(|_| Error::InvalidInput("Model reply".into()))?
            .len();
        if bytes > 20 * 1024 || task.model_bytes + bytes as u64 > 4 * 1024 * 1024 {
            return Err(Error::LimitExceeded("Model response budget".into()));
        }
        task.model_bytes += bytes as u64;
        task.last_model_reply = Some(reply.clone());
        task.state = if reply.tool_call.is_none() {
            TaskState::Completed {
                summary: reply.content.clone(),
            }
        } else {
            TaskState::Running { in_flight: None }
        };
        self.save(
            task,
            TraceEvent::ModelFinished {
                step_id: step_id.into(),
                reply,
            },
        )
    }
    pub fn abort_model_at(
        &self,
        id: &str,
        revision: u64,
        owner: &Principal,
        error: impl Into<String>,
    ) -> Result<Task> {
        let mut task = self.get_owned_at(id, revision, owner)?;
        let TaskState::ModelThinking { step_id, .. } = task.state.clone() else {
            return Err(invalid("No model reservation"));
        };
        let error = error.into();
        bounded_text(&error, 4096, "model failure")?;
        task.state = TaskState::Running { in_flight: None };
        self.save(task, TraceEvent::ModelFailed { step_id, error })
    }

    /// Record inert diagnostic evidence for the exact pending model request.
    /// Rejection neither accepts a reply nor dispatches a proposed tool. The
    /// reserved response allowance also covers this bounded terminal record.
    pub fn reject_model_at(
        &self,
        id: &str,
        revision: u64,
        owner: &Principal,
        step_id: &str,
        reply: crate::RejectedModelReply,
    ) -> Result<Task> {
        let mut task = self.get_owned_at(id, revision, owner)?;
        if !matches!(&task.state, TaskState::ModelThinking { step_id: current, .. } if current == step_id)
        {
            return Err(invalid("No matching model reservation"));
        }
        reply.validate()?;
        let bytes = reply.encoded_len()? as u64;
        task.model_bytes = task
            .model_bytes
            .checked_add(bytes)
            .filter(|total| *total <= 4 * 1024 * 1024)
            .ok_or_else(|| Error::LimitExceeded("Model response budget".into()))?;
        task.last_model_rejection = Some(crate::ModelRejectionRecord {
            step_id: step_id.into(),
            reply: reply.clone(),
        });
        task.state = TaskState::Running { in_flight: None };
        self.save(
            task,
            TraceEvent::ModelRejected {
                step_id: step_id.into(),
                reply,
            },
        )
    }

    pub fn start(&self, id: &str, grant: &ExecutionGrant) -> Result<Task> {
        self.start_task(self.get(id, grant)?)
    }

    pub fn start_at(&self, id: &str, revision: u64, grant: &ExecutionGrant) -> Result<Task> {
        self.start_task(self.get_at(id, revision, grant)?)
    }

    fn start_task(&self, mut task: Task) -> Result<Task> {
        if task.state != TaskState::Pending {
            return Err(invalid("start requires pending"));
        }
        task.state = TaskState::Running { in_flight: None };
        self.save(task, TraceEvent::Started)
    }

    pub fn call(&self, id: &str, grant: &ExecutionGrant, call: ToolCall) -> Result<ToolOutcome> {
        self.call_task(self.get(id, grant)?, grant, call)
    }

    /// Bind a browser/host action to the revision actually reviewed by its
    /// caller. The initial load and later CAS are one transition decision.
    pub fn call_at(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        call: ToolCall,
    ) -> Result<ToolOutcome> {
        self.call_task(self.get_at(id, revision, grant)?, grant, call)
    }

    fn call_task(
        &self,
        mut task: Task,
        grant: &ExecutionGrant,
        call: ToolCall,
    ) -> Result<ToolOutcome> {
        ensure_idle(&task)?;
        if task.tool_calls.len() >= self.registry.limits.max_calls_per_task {
            return Err(Error::LimitExceeded("task tool calls".into()));
        }
        if task.tool_calls.contains(&call.id) {
            return Err(Error::InvalidInput("tool call id already used".into()));
        }
        let request = match self.registry.prepare(grant, &call) {
            Ok(request) => request,
            Err(error) => {
                // Bounded rejection metadata. Untrusted oversized arguments are
                // not copied into the trace or interpreted as authority.
                self.save(
                    task,
                    TraceEvent::ToolRejected {
                        name: call.name.chars().take(128).collect(),
                        error: error.clone(),
                    },
                )?;
                return Err(error);
            }
        };
        task.tool_calls.insert(call.id.clone());
        if request.requires_approval() {
            task.state = TaskState::NeedsApproval { call: call.clone() };
            self.save(task, TraceEvent::ApprovalRequested { call })?;
            return Ok(ToolOutcome::NeedsApproval);
        }
        match request {
            Request::Plan(steps) => {
                task.plan = steps.clone();
                self.save(task, TraceEvent::PlanUpdated { steps })?;
                Ok(ToolOutcome::Returned(json!({"plan_updated":true})))
            }
            Request::Clarify(question) => {
                task.state = TaskState::NeedsClarification {
                    question: question.clone(),
                };
                self.save(task, TraceEvent::ClarificationRequested { question })?;
                Ok(ToolOutcome::NeedsClarification)
            }
            Request::Complete(summary) => {
                task.state = TaskState::Completed { summary };
                self.save(task, TraceEvent::Completed)?;
                Ok(ToolOutcome::Completed)
            }
            request => self.execute(task, grant, call, request, false),
        }
    }

    /// Exact, single-use approval for the already-persisted call. It cannot
    /// grant a missing capability or replace the approved path/content.
    pub fn decide_approval(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        decision: ApprovalDecision,
    ) -> Result<ToolOutcome> {
        let mut task = self.get_at(id, revision, grant)?;
        let TaskState::NeedsApproval { call } = task.state.clone() else {
            return Err(invalid("no pending approval"));
        };
        if decision == ApprovalDecision::Deny {
            task.state = TaskState::Running { in_flight: None };
            self.save(task, TraceEvent::ApprovalDenied { call_id: call.id })?;
            return Ok(ToolOutcome::Failed(Error::Denied("approval denied".into())));
        }
        let request = self.registry.prepare(grant, &call)?;
        self.execute(task, grant, call, request, true)
    }

    pub fn resume_clarification(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        answer: impl Into<String>,
    ) -> Result<Task> {
        let answer = answer.into();
        if answer.trim().is_empty() {
            return Err(Error::InvalidInput("clarification answer is empty".into()));
        }
        bounded_text(&answer, 8192, "clarification answer")?;
        let mut task = self.get_at(id, revision, grant)?;
        if !matches!(task.state, TaskState::NeedsClarification { .. }) {
            return Err(invalid("task is not waiting for clarification"));
        }
        task.clarification_answers.push(answer.clone());
        task.state = TaskState::Running { in_flight: None };
        self.save(task, TraceEvent::ClarificationAnswered { answer })
    }

    /// Cancellation is permanent. If a bounded synchronous operation has
    /// already begun, its syscall can finish, but its late completion cannot
    /// overwrite this terminal state. Pending calls/approvals never execute.
    pub fn cancel(
        &self,
        id: &str,
        grant: &ExecutionGrant,
        reason: impl Into<String>,
    ) -> Result<Task> {
        self.cancel_task(self.get(id, grant)?, reason.into())
    }

    /// Host cancellation needs authenticated ownership, not a live filesystem
    /// handle. It cannot run a tool or add permissions.
    pub fn cancel_owned_at(
        &self,
        id: &str,
        revision: u64,
        owner: &Principal,
        reason: impl Into<String>,
    ) -> Result<Task> {
        self.cancel_task(self.get_owned_at(id, revision, owner)?, reason.into())
    }

    fn cancel_task(&self, mut task: Task, reason: String) -> Result<Task> {
        bounded_text(&reason, 8192, "cancel reason")?;
        if task.state.is_terminal() {
            return Err(invalid("terminal tasks cannot be cancelled"));
        }
        task.state = TaskState::Cancelled { reason };
        self.save(task, TraceEvent::Cancelled)
    }

    pub fn fail(&self, id: &str, grant: &ExecutionGrant, error: impl Into<String>) -> Result<Task> {
        let error = error.into();
        bounded_text(&error, 8192, "failure diagnostic")?;
        let mut task = self.get(id, grant)?;
        if task.state.is_terminal() {
            return Err(invalid("terminal tasks cannot fail again"));
        }
        task.state = TaskState::Failed { error };
        self.save(task, TraceEvent::Failed)
    }

    /// Call only after establishing that the old process/worker has stopped.
    /// Opening a store alone never steals an active task or replays a tool.
    pub fn mark_interrupted(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
    ) -> Result<Task> {
        self.interrupt_task(self.get_at(id, revision, grant)?)
    }

    /// Host startup/lock recovery after proving the previous worker stopped.
    /// This records uncertainty without opening or recreating a workspace.
    pub fn mark_interrupted_owned_at(
        &self,
        id: &str,
        revision: u64,
        owner: &Principal,
    ) -> Result<Task> {
        self.interrupt_task(self.get_owned_at(id, revision, owner)?)
    }

    fn interrupt_task(&self, mut task: Task) -> Result<Task> {
        let in_flight = match task.state.clone() {
            TaskState::Running { in_flight } => in_flight,
            TaskState::ModelThinking { .. } => None,
            _ => return Err(invalid("only running tasks need interruption recovery")),
        };
        task.state = TaskState::Interrupted {
            uncertain_call: in_flight,
        };
        self.save(task, TraceEvent::Interrupted)
    }

    pub fn continue_owned_at(&self, id: &str, revision: u64, owner: &Principal) -> Result<Task> {
        let mut task = self.get_owned_at(id, revision, owner)?;
        if !matches!(task.state, TaskState::Interrupted { .. }) {
            return Err(invalid("task is not interrupted"));
        }
        task.state = TaskState::Running { in_flight: None };
        self.save(task, TraceEvent::ResumedWithoutReplay)
    }

    /// Recovery is explicit. An uncertain write or edit retry returns to approval,
    /// even if the original process recorded approval before it crashed.
    pub fn resume_interrupted(
        &self,
        id: &str,
        revision: u64,
        grant: &ExecutionGrant,
        decision: RecoveryDecision,
    ) -> Result<ToolOutcome> {
        let mut task = self.get_at(id, revision, grant)?;
        let TaskState::Interrupted { uncertain_call } = task.state.clone() else {
            return Err(invalid("task is not interrupted"));
        };
        if decision == RecoveryDecision::ContinueWithoutReplay {
            task.state = TaskState::Running { in_flight: None };
            self.save(task, TraceEvent::ResumedWithoutReplay)?;
            return Ok(ToolOutcome::Returned(
                json!({"resumed":true,"replayed":false}),
            ));
        }
        let call = uncertain_call.ok_or_else(|| invalid("there is no uncertain call to retry"))?;
        let request = self.registry.prepare(grant, &call)?;
        if request.requires_approval() {
            task.state = TaskState::NeedsApproval { call: call.clone() };
            self.save(task, TraceEvent::ApprovalRequested { call })?;
            return Ok(ToolOutcome::NeedsApproval);
        }
        self.execute(task, grant, call, request, false)
    }

    fn execute(
        &self,
        mut task: Task,
        grant: &ExecutionGrant,
        call: ToolCall,
        request: Request,
        approved: bool,
    ) -> Result<ToolOutcome> {
        // Reserve both the start checkpoint and its completion before an
        // operation can change a file. Budget exhaustion must not manufacture
        // an avoidable uncertain side effect.
        if task.revision > 1022 {
            return Err(Error::LimitExceeded(
                "task transition budget cannot record tool completion".into(),
            ));
        }
        task.state = TaskState::Running {
            in_flight: Some(call.clone()),
        };
        let mut task = self.save(
            task,
            TraceEvent::ToolStarted {
                call: call.clone(),
                approved,
            },
        )?;
        // A second read observes cancellation committed before execution.
        let current = self.get_at(&task.id, task.revision, grant)?;
        if current.state != task.state {
            return Err(Error::Conflict);
        }
        let discovery = matches!(&request, Request::Discover);
        let mut result =
            self.registry
                .execute(grant, request, &task.id, task.revision, &mut || {
                    self.get_at(&task.id, task.revision, grant)?;
                    Ok(())
                });
        if discovery {
            if let Ok(value) = &mut result {
                for (key, kind) in [
                    ("skills", crate::InstructionKind::Skill),
                    ("methods", crate::InstructionKind::Method),
                    ("plugins", crate::InstructionKind::Plugin),
                ] {
                    value[key]=json!(task.extensions.items.iter().filter(|item|item.kind==kind).map(|item|json!({"id":item.id,"revision":item.revision,"name":item.name,"description":item.description})).collect::<Vec<_>>());
                }
                value["instruction_source"] = json!("immutable_task_capture");
            }
        }
        task.state = if matches!(result, Err(Error::UncertainSideEffect(_))) {
            TaskState::Interrupted {
                uncertain_call: Some(call.clone()),
            }
        } else {
            TaskState::Running { in_flight: None }
        };
        task.last_result = result.as_ref().ok().cloned();
        let event = TraceEvent::ToolFinished {
            call_id: call.id,
            result: result.clone(),
        };
        // Failed persistence here leaves durable in-flight evidence. Do not
        // claim completion, rewind, or automatically replay the side effect.
        self.save(task, event)?;
        Ok(match result {
            Ok(value) => ToolOutcome::Returned(value),
            Err(error) => ToolOutcome::Failed(error),
        })
    }

    fn get_at(&self, id: &str, revision: u64, grant: &ExecutionGrant) -> Result<Task> {
        let task = self.get(id, grant)?;
        if task.revision != revision {
            return Err(Error::Conflict);
        }
        Ok(task)
    }
    fn get_owned_at(&self, id: &str, revision: u64, owner: &Principal) -> Result<Task> {
        let task = self.get_owned(id, owner)?;
        if task.revision != revision {
            return Err(Error::Conflict);
        }
        Ok(task)
    }
    fn save(&self, mut task: Task, event: TraceEvent) -> Result<Task> {
        let host_resolution = matches!(
            event,
            TraceEvent::Interrupted
                | TraceEvent::ResumedWithoutReplay
                | TraceEvent::ClarificationAnswered { .. }
                | TraceEvent::ApprovalDenied { .. }
        );
        if task.revision >= 1024 && !task.state.is_terminal() && !host_resolution {
            return Err(Error::LimitExceeded("task transitions".into()));
        }
        let previous = task.revision;
        task.revision += 1;
        self.store.commit(Some(previous), &task, &event)?;
        Ok(task)
    }
}
fn invalid(message: &str) -> Error {
    Error::InvalidTransition(message.into())
}
fn ensure_idle(task: &Task) -> Result<()> {
    if task.state == (TaskState::Running { in_flight: None }) {
        Ok(())
    } else {
        Err(invalid("task must be running with no call in flight"))
    }
}
