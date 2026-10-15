use serde_json::{json, Value};
use smith_agent_core::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct Adapter(Arc<AtomicUsize>);
impl HostToolAdapter for Adapter {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            name: "save_artifact".into(),
            source: "test:counted-effect".into(),
            parameters: json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"],"additionalProperties":false}),
            availability: Availability::Implemented,
            required_capability: Some(Capability::SaveArtifact),
            requires_approval: true,
        }]
    }
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> Result<()> {
        if !grant.permits_resource(ResourceKind::ArtifactCollection, "unowned") {
            return Err(Error::Denied("scope".into()));
        }
        if call.arguments.as_object().is_none_or(|o| o.len() != 1)
            || !call.arguments["title"].is_string()
        {
            return Err(Error::InvalidInput("title only".into()));
        }
        Ok(())
    }
    fn execute(
        &self,
        c: &HostToolContext<'_>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        check()?;
        assert!(!c.task_id.is_empty());
        assert!(c.started_revision >= 3);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"saved":true,"title":c.call.arguments["title"]}))
    }
}
fn grant(caps: Vec<Capability>, resources: Vec<ResourceScope>) -> ExecutionGrant {
    ExecutionGrant::new_scoped(
        Principal::new("host-user").unwrap(),
        Arc::new(Workspace::create_temporary().unwrap()),
        "display label",
        caps,
        resources,
    )
    .unwrap()
}
fn resource(kind: ResourceKind, id: &str) -> ResourceScope {
    ResourceScope {
        kind,
        id: id.into(),
    }
}
#[test]
fn registration_is_truthful_and_cannot_override_or_mint_dynamic_names() {
    let count = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::default()
        .with_adapter(Arc::new(Adapter(count.clone())))
        .unwrap();
    assert_eq!(registry.all_tools().len(), 35);
    assert_eq!(
        registry
            .all_tools()
            .iter()
            .filter(|d| d.availability == Availability::Implemented)
            .count(),
        10
    );
    assert!(registry.with_adapter(Arc::new(Adapter(count))).is_err());
}
#[test]
fn scoped_delegation_and_legacy_manifests_do_not_broaden_authority() {
    let original = grant(
        vec![Capability::SaveArtifact],
        vec![resource(ResourceKind::ArtifactCollection, "unowned")],
    );
    let child = original
        .delegate_scoped(
            "another persona",
            [Capability::SaveArtifact, Capability::WriteMemory],
            [
                resource(ResourceKind::ArtifactCollection, "unowned"),
                resource(ResourceKind::MemoryNamespace, "private"),
            ],
        )
        .unwrap();
    assert!(child.permits(Capability::SaveArtifact));
    assert!(!child.permits(Capability::WriteMemory));
    assert!(!child.permits_resource(ResourceKind::MemoryNamespace, "private"));
    let mut legacy = serde_json::to_value(original.manifest()).unwrap();
    legacy.as_object_mut().unwrap().remove("resources");
    let restored: GrantManifest = serde_json::from_value(legacy).unwrap();
    assert!(restored.resources.is_empty());
}
#[test]
fn host_effect_waits_for_exact_approval_and_cancellation_cannot_run_it() {
    let count = Arc::new(AtomicUsize::new(0));
    let store = MemoryTraceStore::default();
    let registry = ToolRegistry::default()
        .with_adapter(Arc::new(Adapter(count.clone())))
        .unwrap();
    let engine = TaskEngine::new(store.clone(), registry);
    let grant = grant(
        vec![Capability::SaveArtifact],
        vec![resource(ResourceKind::ArtifactCollection, "unowned")],
    );
    let task = engine.create(&grant, "artifact").unwrap();
    let task = engine.start_at(&task.id, 0, &grant).unwrap();
    assert_eq!(
        engine
            .call_at(
                &task.id,
                task.revision,
                &grant,
                ToolCall::new("save_artifact", json!({"title":"synthetic"}))
            )
            .unwrap(),
        ToolOutcome::NeedsApproval
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(engine
        .decide_approval(&task.id, task.revision, &grant, ApprovalDecision::Approve)
        .is_err());
    let task = store.load(&task.id).unwrap();
    engine
        .decide_approval(&task.id, task.revision, &grant, ApprovalDecision::Approve)
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let task = store.load(&task.id).unwrap();
    engine
        .call_at(
            &task.id,
            task.revision,
            &grant,
            ToolCall::new("save_artifact", json!({"title":"second"})),
        )
        .unwrap();
    let task = store.load(&task.id).unwrap();
    engine
        .cancel_owned_at(&task.id, task.revision, grant.principal(), "cancel")
        .unwrap();
    assert!(engine
        .decide_approval(&task.id, task.revision, &grant, ApprovalDecision::Approve)
        .is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[test]
fn host_argument_scope_and_capability_rejections_never_start_an_effect() {
    let count = Arc::new(AtomicUsize::new(0));
    let store = MemoryTraceStore::default();
    let engine = TaskEngine::new(
        store.clone(),
        ToolRegistry::default()
            .with_adapter(Arc::new(Adapter(count.clone())))
            .unwrap(),
    );
    let grant = grant(vec![Capability::SaveArtifact], vec![]);
    let task = engine.create(&grant, "no scope").unwrap();
    engine.start(&task.id, &grant).unwrap();
    assert!(matches!(
        engine.call(
            &task.id,
            &grant,
            ToolCall::new("save_artifact", json!({"title":"x","grant":"unowned"}))
        ),
        Err(Error::Denied(_))
    ));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!store
        .events(&task.id)
        .unwrap()
        .iter()
        .any(|e| matches!(e.event, TraceEvent::ToolStarted { .. })));
}
#[test]
fn model_reservation_is_scoped_durable_and_recovery_does_not_replay() {
    let store = MemoryTraceStore::default();
    let engine = TaskEngine::new(store.clone(), ToolRegistry::default());
    let grant = grant(
        vec![Capability::ModelInference],
        vec![resource(ResourceKind::ProviderBinding, "target-a")],
    );
    let task = engine.create(&grant, "model task").unwrap();
    engine.start(&task.id, &grant).unwrap();
    assert!(matches!(
        engine.begin_model_at(
            &task.id,
            1,
            &grant,
            "target-b",
            "binding",
            json!({}),
            json!({})
        ),
        Err(Error::Denied(_))
    ));
    let task = engine
        .begin_model_at(
            &task.id,
            1,
            &grant,
            "target-a",
            "binding",
            json!({"input":"synthetic"}),
            json!({"model":"fixture"}),
        )
        .unwrap();
    assert_eq!(task.model_steps, 1);
    assert!(matches!(task.state, TaskState::ModelThinking { .. }));
    let task = engine
        .mark_interrupted_owned_at(&task.id, task.revision, grant.principal())
        .unwrap();
    assert_eq!(
        task.state,
        TaskState::Interrupted {
            uncertain_call: None
        }
    );
    let task = engine
        .continue_owned_at(&task.id, task.revision, grant.principal())
        .unwrap();
    assert_eq!(task.model_steps, 1);
    assert!(store
        .events(&task.id)
        .unwrap()
        .iter()
        .any(|e| matches!(e.event, TraceEvent::ModelStarted { .. })));
}
#[test]
fn late_or_oversized_model_replies_cannot_dispatch_or_overwrite_cancellation() {
    let store = MemoryTraceStore::default();
    let engine = TaskEngine::new(store.clone(), ToolRegistry::default());
    let grant = grant(
        vec![Capability::ModelInference],
        vec![resource(ResourceKind::ProviderBinding, "target-a")],
    );
    let task = engine.create(&grant, "model").unwrap();
    engine.start(&task.id, &grant).unwrap();
    let task = engine
        .begin_model_at(
            &task.id,
            1,
            &grant,
            "target-a",
            "binding",
            json!({}),
            json!({}),
        )
        .unwrap();
    let TaskState::ModelThinking { step_id, .. } = &task.state else {
        panic!()
    };
    assert!(engine
        .finish_model_at(
            &task.id,
            task.revision,
            &grant,
            step_id,
            ModelReply {
                content: "x".repeat(17000),
                tool_call: None,
                provenance: json!({})
            }
        )
        .is_err());
    engine
        .cancel_owned_at(&task.id, task.revision, grant.principal(), "stop")
        .unwrap();
    assert!(matches!(
        engine.finish_model_at(
            &task.id,
            task.revision,
            &grant,
            step_id,
            ModelReply {
                content: "late".into(),
                tool_call: Some(ToolCall::new("run_shell", json!({}))),
                provenance: json!({})
            }
        ),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        store.load(&task.id).unwrap().state,
        TaskState::Cancelled { .. }
    ));
}
#[test]
fn model_proposal_is_data_until_separate_dispatch_and_budget_is_monotonic() {
    let store = MemoryTraceStore::default();
    let engine = TaskEngine::new(store.clone(), ToolRegistry::default());
    let grant = grant(
        vec![Capability::ModelInference],
        vec![resource(ResourceKind::ProviderBinding, "target-a")],
    );
    let task = engine.create(&grant, "model").unwrap();
    engine.start(&task.id, &grant).unwrap();
    let task = engine
        .begin_model_at(
            &task.id,
            1,
            &grant,
            "target-a",
            "binding",
            json!({}),
            json!({}),
        )
        .unwrap();
    let TaskState::ModelThinking { step_id, .. } = &task.state else {
        panic!()
    };
    let task = engine
        .finish_model_at(
            &task.id,
            task.revision,
            &grant,
            step_id,
            ModelReply {
                content: "proposal only".into(),
                tool_call: Some(ToolCall::new("run_shell", json!({"command":"whoami"}))),
                provenance: json!({}),
            },
        )
        .unwrap();
    assert_eq!(task.state, TaskState::Running { in_flight: None });
    assert!(task.tool_calls.is_empty());
    assert_eq!(task.model_steps, 1);
    assert!(!store
        .events(&task.id)
        .unwrap()
        .iter()
        .any(|r| matches!(r.event, TraceEvent::ToolStarted { .. })));
    let mut exhausted = task.clone();
    exhausted.model_steps = 64;
    exhausted.revision += 1;
    store
        .commit(
            Some(task.revision),
            &exhausted,
            &TraceEvent::ResumedWithoutReplay,
        )
        .unwrap();
    assert!(matches!(
        engine.begin_model_at(
            &task.id,
            exhausted.revision,
            &grant,
            "target-a",
            "binding",
            json!({}),
            json!({})
        ),
        Err(Error::LimitExceeded(_))
    ));
}
