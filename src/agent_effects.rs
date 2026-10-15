//! Exact host plans for process, Git and network effects. Browser JSON is review
//! data; only the host retains executable policy and single-use approval tokens.
use super::*;
use smith_agent_core::{
    ExecutionGrant, HostToolContext, ResourceKind, ToolDescriptor, WorkspaceIdentity,
};
use smith_tool_effects::{
    ApprovedPlan, BindingSummary, DestinationGrant, EffectHost, ExecutableGrant, NetworkOperation,
    PlanSummary, WorkspaceGrant,
};
use std::{collections::BTreeMap, net::SocketAddr, path::Path};

#[derive(Default)]
pub(super) struct Effects {
    host: EffectHost,
    bindings: Mutex<BTreeMap<String, Option<WorkspaceIdentity>>>,
    plans: Mutex<BTreeMap<String, PlanSummary>>,
    approved: Mutex<BTreeMap<(String, String), (ToolCall, ApprovedPlan)>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedCall {
    plan_id: String,
    review: Value,
}
fn unavailable() -> Error {
    Error::Persistence("Effect authority unavailable".into())
}
fn effect_error(e: smith_tool_effects::Error) -> Error {
    use smith_tool_effects::Error as E;
    match e {
        E::Invalid(m) => Error::InvalidInput(m.into()),
        E::Unsupported(m) => Error::Unsupported(m.into()),
        E::Limit(m) => Error::LimitExceeded(m.into()),
        E::Stale => Error::Conflict,
        other => Error::Denied(format!("Reviewed effect rejected: {other}")),
    }
}
impl Effects {
    #[cfg(test)]
    pub(super) fn fixture_destination(
        &self,
        url: &str,
        address: SocketAddr,
        operation: NetworkOperation,
    ) -> smith_agent_core::Result<Value> {
        let binding = self
            .host
            .bind_destination(
                DestinationGrant::loopback_fixture(url, address, operation)
                    .map_err(effect_error)?,
            )
            .map_err(effect_error)?;
        self.remember(&binding, None)?;
        Ok(json!(binding))
    }

    fn remember(
        &self,
        binding: &BindingSummary,
        workspace: Option<WorkspaceIdentity>,
    ) -> smith_agent_core::Result<()> {
        self.bindings
            .lock()
            .map_err(|_| unavailable())?
            .insert(binding.id.clone(), workspace);
        Ok(())
    }
    fn prepare(
        &self,
        binding: &str,
        revision: u64,
        tool: &str,
        arguments: Value,
    ) -> smith_agent_core::Result<Value> {
        let plan = self
            .host
            .prepare(binding, revision, tool, arguments)
            .map_err(effect_error)?;
        let summary = plan.summary().clone();
        let mut plans = self.plans.lock().map_err(|_| unavailable())?;
        // EffectHost bounds live plans. Retain only a bounded review cache too.
        if plans.len() >= 128 {
            return Err(Error::LimitExceeded(
                "Effect review cache; recreate the profile session".into(),
            ));
        }
        plans.insert(summary.id.clone(), summary.clone());
        Ok(
            json!({"plan":summary,"call":{"name":tool,"arguments":{"plan_id":summary.id,"review":summary}}}),
        )
    }
    fn plan(&self, call: &ToolCall) -> smith_agent_core::Result<PlanSummary> {
        let a: PlannedCall = serde_json::from_value(call.arguments.clone())
            .map_err(|_| Error::InvalidInput("Use the exact reviewed effect plan".into()))?;
        let summary = self
            .plans
            .lock()
            .map_err(|_| unavailable())?
            .get(&a.plan_id)
            .cloned()
            .ok_or_else(|| {
                Error::Denied("Effect plan expired or belongs to a previous profile session".into())
            })?;
        if summary.tool != call.name || a.review != json!(summary) {
            return Err(Error::Denied(
                "Effect review does not match the retained plan".into(),
            ));
        }
        Ok(summary)
    }
    fn approve(&self, task: &str, call: &ToolCall) -> smith_agent_core::Result<()> {
        let plan = self.plan(call)?;
        let approval = self
            .host
            .approve(&plan.id, plan.revision)
            .map_err(effect_error)?;
        if json!(approval.summary()) != json!(plan) {
            return Err(Error::Conflict);
        }
        self.approved
            .lock()
            .map_err(|_| unavailable())?
            .insert((task.into(), call.id.clone()), (call.clone(), approval));
        Ok(())
    }
    fn clear_approval(&self, task: &str, call: &str) {
        if let Ok(mut a) = self.approved.lock() {
            a.remove(&(task.into(), call.into()));
        }
    }
    pub(super) fn shutdown(&self) {
        if let Ok(mut approved) = self.approved.lock() {
            approved.clear();
        }
        if let Ok(mut plans) = self.plans.lock() {
            plans.clear();
        }
        if let Ok(mut bindings) = self.bindings.lock() {
            for id in bindings.keys() {
                let _ = self.host.revoke(id);
            }
            bindings.clear();
        }
    }
}
impl HostToolAdapter for Effects {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        [("run_shell",Capability::Shell),("git_clone",Capability::GitWrite),("git_commit",Capability::GitWrite),("git_diff",Capability::GitRead),("git_blame",Capability::GitRead),("git_status",Capability::GitRead),("git_log",Capability::GitRead),("web_fetch",Capability::WebFetch),("send_webhook",Capability::SendWebhook)].into_iter().map(|(name,cap)|ToolDescriptor{
            name:name.into(),source:"host:reviewed-tool-effects-v1".into(),parameters:json!({"type":"object","properties":{"plan_id":{"type":"string"},"review":{"type":"object"}},"required":["plan_id","review"],"additionalProperties":false}),availability:if cfg!(target_os="macos")||matches!(name,"web_fetch"|"send_webhook"){Availability::Implemented}else{Availability::Unsupported("Confined process backend is available only on macOS".into())},required_capability:Some(cap),requires_approval:true,
        }).collect()
    }
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> smith_agent_core::Result<()> {
        let summary = self.plan(call)?;
        if !grant.permits_resource(ResourceKind::EffectBinding, &summary.binding_id) {
            return Err(Error::Denied(
                "Exact effect binding is not granted to this task".into(),
            ));
        }
        if let Some(source) = summary.policy["clone_source"]["binding_id"].as_str() {
            if !grant.permits_resource(ResourceKind::EffectBinding, source) {
                return Err(Error::Denied(
                    "Clone source binding is not granted to this task".into(),
                ));
            }
        }
        let bindings = self.bindings.lock().map_err(|_| unavailable())?;
        let identity = bindings
            .get(&summary.binding_id)
            .ok_or_else(|| Error::Denied("Effect binding is no longer active".into()))?;
        if identity
            .as_ref()
            .is_some_and(|identity| identity != &grant.manifest().workspace)
        {
            return Err(Error::Denied(
                "Effect binding belongs to another workspace".into(),
            ));
        }
        Ok(())
    }
    fn execute(
        &self,
        context: &HostToolContext<'_>,
        check_active: &mut dyn FnMut() -> smith_agent_core::Result<()>,
    ) -> smith_agent_core::Result<Value> {
        self.validate(context.grant, context.call)?;
        check_active()?;
        let key = (context.task_id.into(), context.call.id.clone());
        let (call, approval) = self
            .approved
            .lock()
            .map_err(|_| unavailable())?
            .remove(&key)
            .ok_or_else(|| {
                Error::Denied(
                    "Explicit user approval of this exact retained effect plan is required".into(),
                )
            })?;
        if call != *context.call {
            return Err(Error::Denied("Approved effect call mismatch".into()));
        }
        // After consumption, conservative uncertainty prevents automatic replay
        // if a process/network operation fails after it may have had an effect.
        self.host
            .execute(approval, &mut || {
                check_active().map_err(|_| smith_tool_effects::Error::Cancelled)
            })
            .map_err(|e| {
                Error::UncertainSideEffect(format!(
                    "Reviewed effect consumed; inspect before any retry: {e}"
                ))
            })
    }
}

pub(super) fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/agent/effects/catalog", get(catalog))
        .route("/api/agent/effects/bindings", get(bindings))
        .route("/api/agent/effects/bind-workspace", post(bind_workspace))
        .route("/api/agent/effects/bind-network", post(bind_network))
        .route("/api/agent/effects/prepare", post(prepare))
        .route("/api/agent/effects/approve", post(approve))
        .route("/api/agent/effects/provision", post(provision_git))
        .route("/api/agent/effects/audit", get(audit))
}
const GIT: &str = "/Applications/Xcode.app/Contents/Developer/usr/bin/git";
fn catalog_binary() -> smith_agent_core::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("SMITH_TEST_CATALOG_BINARY") {
        return std::fs::canonicalize(path).map_err(|e| Error::Io(e.to_string()));
    }
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| Error::Io(e.to_string()))
}
fn catalog_executable(name: &str) -> smith_agent_core::Result<ExecutableGrant> {
    if name != "echo" {
        return Err(Error::Denied(
            "Executable is not in the host catalog".into(),
        ));
    }
    ExecutableGrant::inspect_with_prefix(
        name,
        &catalog_binary()?,
        vec![crate::catalog_mode::ECHO_MODE.into()],
    )
    .map_err(effect_error)
}
async fn catalog() -> Json<Value> {
    Json(
        json!({"process_backend":if cfg!(target_os="macos"){"macos_sandbox_exec"}else{"unsupported"},"executables":[{"id":"echo","path":catalog_binary().ok(),"provenance":"Smith built-in bounded echo mode; no shell, environment, profile or network access","argv_prefix":[crate::catalog_mode::ECHO_MODE],"max_output_bytes":8192}],"git":{"path":GIT,"available":Path::new(GIT).is_file()},"network":"Exact HTTPS URL and public pinned socket address; no DNS, redirects or proxy","tools":smith_tool_effects::descriptors()}),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindWorkspace {
    workspace_id: String,
    executables: Vec<String>,
    git: bool,
    confirmed: bool,
}
async fn bind_workspace(
    State(app): State<Arc<App>>,
    input: Body<BindWorkspace>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied("Confirm the executable and workspace grant".into()).into());
    }
    work(app,None,move |app|{
        let store=AgentStore::new(&app)?;let policy=store.policy(&input.workspace_id)?;
        let workspace=Workspace::reopen_temporary(&policy.identity)?;
        let path=workspace.host_path().canonicalize().map_err(|e| Error::Io(e.to_string()))?;
        let grant=WorkspaceGrant::owned_temporary(&path,Some((policy.identity.device,policy.identity.inode))).map_err(effect_error)?;
        if input.executables.len()>1{return Err(Error::InvalidInput("Duplicate or unsupported executable selection".into()).into());}
        let executables=input.executables.iter().map(|name|catalog_executable(name)).collect::<smith_agent_core::Result<Vec<_>>>()?;
        let git=if input.git{Some(ExecutableGrant::inspect("git",Path::new(GIT)).map_err(effect_error)?)}else{None};
        ensure_open(&app)?;
        let binding=app.agent.effects.host.bind_workspace(grant,executables,git).map_err(effect_error)?;
        app.agent.effects.remember(&binding,Some(policy.identity))?;
        Ok(Json(json!({"binding":binding,"resource":{"kind":"effect_binding","id":binding.id},"git_initialized":false})))
    }).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindNetwork {
    url: String,
    pinned_address: SocketAddr,
    operation: NetworkOperation,
    confirmed: bool,
}
async fn bind_network(
    State(app): State<Arc<App>>,
    input: Body<BindNetwork>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(
            Error::Denied("Confirm the exact network destination and operation".into()).into(),
        );
    }
    work(app, None, move |app| {
        ensure_open(&app)?;
        let grant = DestinationGrant::public(&input.url, input.pinned_address, input.operation)
            .map_err(effect_error)?;
        let binding = app
            .agent
            .effects
            .host
            .bind_destination(grant)
            .map_err(effect_error)?;
        app.agent.effects.remember(&binding, None)?;
        Ok(Json(
            json!({"binding":binding,"resource":{"kind":"effect_binding","id":binding.id}}),
        ))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    binding_id: String,
    revision: u64,
    tool: String,
    arguments: Value,
}
async fn prepare(State(app): State<Arc<App>>, input: Body<Prepare>) -> AResult<Json<Value>> {
    let input = body(input)?;
    work(app, None, move |app| {
        ensure_open(&app)?;
        Ok(Json(app.agent.effects.prepare(
            &input.binding_id,
            input.revision,
            &input.tool,
            input.arguments,
        )?))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approve {
    task_id: String,
    expected_revision: u64,
    call_id: String,
    plan_id: String,
    confirmed: bool,
}
async fn approve(State(app): State<Arc<App>>, input: Body<Approve>) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied("Confirm this exact effect plan".into()).into());
    }
    work(app, Some(input.task_id.clone()), move |app| {
        let store = AgentStore::new(&app)?;
        let grant = store.grant_for_task(&input.task_id)?;
        let engine = TaskEngine::new(store.clone(), store.registry()?);
        let task = store.load(&input.task_id)?;
        let TaskState::NeedsApproval { call } = task.state else {
            return Err(Error::Conflict.into());
        };
        if task.revision != input.expected_revision
            || call.id != input.call_id
            || call.arguments["plan_id"] != input.plan_id
        {
            return Err(Error::Conflict.into());
        }
        app.agent.effects.validate(&grant, &call)?;
        ensure_open(&app)?;
        app.agent.effects.approve(&input.task_id, &call)?;
        let result = engine.decide_approval(
            &input.task_id,
            input.expected_revision,
            &grant,
            ApprovalDecision::Approve,
        );
        app.agent
            .effects
            .clear_approval(&input.task_id, &input.call_id);
        let outcome = result?;
        task_response(&store, &input.task_id, Some(outcome))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvisionGit {
    plan_id: String,
    review: Value,
    confirmed: bool,
}
async fn provision_git(
    State(app): State<Arc<App>>,
    input: Body<ProvisionGit>,
) -> AResult<Json<Value>> {
    let input = body(input)?;
    if !input.confirmed {
        return Err(Error::Denied(
            "Review and confirm initialization of the owned empty repository".into(),
        )
        .into());
    }
    work(app, None, move |app| {
        let effects = &app.agent.effects;
        let call = ToolCall::new(
            "git_init",
            json!({"plan_id":input.plan_id,"review":input.review}),
        );
        let plan = effects.plan(&call)?;
        ensure_open(&app)?;
        let store = AgentStore::new(&app)?;
        let audit_id = store.begin_host_operation("git_init", &json!(plan))?;
        let approval = effects
            .host
            .approve(&plan.id, plan.revision)
            .map_err(effect_error);
        let outcome = match approval {
            Ok(approval) => effects
                .host
                .execute(approval, &mut || {
                    ensure_open(&app).map_err(|_| smith_tool_effects::Error::Cancelled)
                })
                .map_err(|e| {
                    Error::UncertainSideEffect(format!(
                        "Repository provisioning may have happened: {e}"
                    ))
                }),
            Err(e) => Err(e),
        };
        let (state, result) = match &outcome {
            Ok(value) => ("finished", value.clone()),
            Err(Error::UncertainSideEffect(_)) => {
                ("uncertain", json!({"inspect_before_retry":true}))
            }
            Err(e) => ("failed", json!({"error":e.to_string()})),
        };
        store.finish_host_operation(&audit_id, state, &result)?;
        outcome?;
        let binding = effects
            .host
            .binding(&plan.binding_id)
            .map_err(effect_error)?;
        Ok(Json(
            json!({"operation_id":audit_id,"result":result,"binding":binding}),
        ))
    })
    .await
}
async fn audit(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    Ok(Json(
        json!({"operations":AgentStore::new(&app)?.host_operations()?}),
    ))
}

async fn bindings(State(app): State<Arc<App>>) -> AResult<Json<Value>> {
    work(app, None, move |app| {
        let ids = app
            .agent
            .effects
            .bindings
            .lock()
            .map_err(|_| unavailable())?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let values = ids
            .iter()
            .map(|id| app.agent.effects.host.binding(id).map_err(effect_error))
            .collect::<smith_agent_core::Result<Vec<_>>>()?;
        Ok(Json(json!({"bindings":values})))
    })
    .await
}
