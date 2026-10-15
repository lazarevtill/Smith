use serde_json::{json, Value};
use smith_agent_core::*;
use smith_platform::{
    fixture::*, Action, PlatformSession, PlatformToolAdapter, TargetPermissions, TargetRequest,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Fixture {
    session: Arc<Mutex<PlatformSession>>,
    adapter: Arc<PlatformToolAdapter>,
    control: FixtureControl,
    engine: TaskEngine<MemoryTraceStore>,
    grant: ExecutionGrant,
    workspace_path: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let (backend, control) = FixtureBackend::new();
        let mut session = PlatformSession::new("owner", Box::new(backend)).unwrap();
        session
            .select_target(
                TargetRequest::SyntheticFixture,
                TargetPermissions {
                    read_elements: true,
                    interact: true,
                },
            )
            .unwrap();
        let workspace = Arc::new(Workspace::create_temporary().unwrap());
        let workspace_path = workspace.host_path().to_owned();
        let grant = ExecutionGrant::new_scoped(
            Principal::new("owner").unwrap(),
            workspace,
            "fixture",
            [
                Capability::ComputerUse,
                Capability::SystemMonitor,
                Capability::TaskControl,
            ],
            [ResourceScope {
                kind: ResourceKind::EffectBinding,
                id: session.binding().into(),
            }],
        )
        .unwrap();
        let session = Arc::new(Mutex::new(session));
        let adapter = Arc::new(PlatformToolAdapter::new(session.clone()));
        let engine = TaskEngine::new(
            MemoryTraceStore::default(),
            ToolRegistry::default()
                .with_adapter(adapter.clone())
                .unwrap(),
        );
        Self {
            session,
            adapter,
            control,
            engine,
            grant,
            workspace_path,
        }
    }
    fn plan_click(&self) -> (String, ToolCall) {
        let mut session = self.session.lock().unwrap();
        let snapshot = session.screen_elements().unwrap();
        let plan = session
            .prepare(&snapshot.id, Action::Click { x: 30, y: 30 })
            .unwrap();
        let call = ToolCall::new(
            "screen_click",
            json!({"x":30,"y":30,"plan_id":plan.id,"review":plan}),
        );
        (plan.id, call)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace_path);
    }
}
#[test]
fn task_engine_exact_approval_and_platform_approval_execute_one_call() {
    let f = Fixture::new();
    let task = f
        .engine
        .create(&f.grant, "synthetic platform journey")
        .unwrap();
    f.engine.start(&task.id, &f.grant).unwrap();
    let monitor = f
        .engine
        .call(
            &task.id,
            &f.grant,
            ToolCall::new("system_monitor", json!({})),
        )
        .unwrap();
    assert!(
        matches!(monitor, ToolOutcome::Returned(ref v) if v["execution_kind"] == "synthetic_fixture")
    );
    let elements = f
        .engine
        .call(
            &task.id,
            &f.grant,
            ToolCall::new("screen_elements", json!({})),
        )
        .unwrap();
    assert!(
        matches!(elements, ToolOutcome::Returned(ref v) if v["elements"].as_str().unwrap().contains("Fixture Save"))
    );
    let (plan, call) = f.plan_click();
    assert_eq!(
        f.engine.call(&task.id, &f.grant, call.clone()).unwrap(),
        ToolOutcome::NeedsApproval
    );
    assert!(f.control.actions().is_empty());
    f.adapter.approve_for_call(&plan, &task.id, &call).unwrap();
    let pending = f.engine.get(&task.id, &f.grant).unwrap();
    let output = f
        .engine
        .decide_approval(
            &task.id,
            pending.revision,
            &f.grant,
            ApprovalDecision::Approve,
        )
        .unwrap();
    assert!(
        matches!(output, ToolOutcome::Returned(ref v) if v["ok"] == true && v["execution_kind"] == "synthetic_fixture")
    );
    assert_eq!(f.control.actions().len(), 1);
    assert!(f
        .engine
        .decide_approval(
            &task.id,
            pending.revision,
            &f.grant,
            ApprovalDecision::Approve
        )
        .is_err());
    assert_eq!(f.control.actions().len(), 1);
}
#[test]
fn task_approval_without_platform_confirmation_denies_native_dispatch() {
    let f = Fixture::new();
    let task = f
        .engine
        .create(&f.grant, "approval is not global authority")
        .unwrap();
    f.engine.start(&task.id, &f.grant).unwrap();
    let (_, call) = f.plan_click();
    assert_eq!(
        f.engine.call(&task.id, &f.grant, call).unwrap(),
        ToolOutcome::NeedsApproval
    );
    let pending = f.engine.get(&task.id, &f.grant).unwrap();
    assert!(matches!(
        f.engine
            .decide_approval(
                &task.id,
                pending.revision,
                &f.grant,
                ApprovalDecision::Approve
            )
            .unwrap(),
        ToolOutcome::Failed(Error::Denied(_))
    ));
    assert!(f.control.actions().is_empty());
}
#[test]
fn scope_principal_and_capability_cannot_be_invented_in_tool_arguments() {
    let f = Fixture::new();
    let call = ToolCall::new("system_monitor", json!({}));
    let scoped = f
        .grant
        .delegate_scoped("unscoped", [Capability::SystemMonitor], [])
        .unwrap();
    assert!(matches!(
        f.adapter.validate(&scoped, &call),
        Err(Error::Denied(_))
    ));
    let no_capability = f.grant.delegate("read-only", []).unwrap();
    assert!(matches!(
        f.adapter.validate(&no_capability, &call),
        Err(Error::Denied(_))
    ));
    let mut forged = ToolCall::new(
        "screen_elements",
        json!({"target":"anything","grant":true,"principal":"owner"}),
    );
    assert!(matches!(
        f.adapter.validate(&f.grant, &forged),
        Err(Error::InvalidInput(_))
    ));
    forged.arguments = Value::Null;
    assert!(matches!(
        f.adapter.validate(&f.grant, &forged),
        Err(Error::InvalidInput(_))
    ));
    f.session.lock().unwrap().revoke();
    assert!(matches!(
        f.adapter.validate(&f.grant, &call),
        Err(Error::Denied(_))
    ));
    assert_eq!(f.control.frame_reads(), 0);
}
#[test]
fn wrong_action_call_or_task_cannot_use_retained_approval() {
    let f = Fixture::new();
    let (plan, call) = f.plan_click();
    let mut altered = call.clone();
    altered.arguments["x"] = json!(31);
    assert!(f
        .adapter
        .approve_for_call(&plan, "task-a", &altered)
        .is_err());
    altered.arguments["x"] = json!(i64::MAX);
    assert!(matches!(
        f.adapter.validate(&f.grant, &altered),
        Err(Error::InvalidInput(_))
    ));
    f.adapter.approve_for_call(&plan, "task-a", &call).unwrap();
    let context = HostToolContext {
        task_id: "task-b",
        started_revision: 1,
        call: &call,
        grant: &f.grant,
    };
    assert!(matches!(
        f.adapter.execute(&context, &mut || Ok(())),
        Err(Error::Denied(_))
    ));
    let renamed = ToolCall::new(&call.name, call.arguments.clone());
    let context = HostToolContext {
        task_id: "task-a",
        started_revision: 1,
        call: &renamed,
        grant: &f.grant,
    };
    assert!(matches!(
        f.adapter.execute(&context, &mut || Ok(())),
        Err(Error::Denied(_))
    ));
    assert!(f.control.actions().is_empty());
}

#[test]
fn durable_plan_review_is_evidence_only_and_must_match_retained_plan() {
    let f = Fixture::new();
    let (plan, call) = f.plan_click();
    let mut altered = call.clone();
    altered.arguments["review"]["target"]["id"] = json!("forged-other-window");
    assert!(matches!(
        f.adapter.validate(&f.grant, &altered),
        Err(Error::Denied(_))
    ));
    assert!(f
        .adapter
        .approve_for_call(&plan, "task-a", &altered)
        .is_err());
    let mut missing = call.clone();
    missing.arguments.as_object_mut().unwrap().remove("review");
    assert!(matches!(
        f.adapter.validate(&f.grant, &missing),
        Err(Error::InvalidInput(_))
    ));
    f.session.lock().unwrap().revoke();
    assert!(f.adapter.approve_for_call(&plan, "task-a", &call).is_err());
    assert!(f.control.actions().is_empty());
}
