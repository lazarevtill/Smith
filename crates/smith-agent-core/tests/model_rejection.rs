use serde_json::json;
use smith_agent_core::*;
use std::sync::Arc;

struct Fixture {
    engine: TaskEngine<MemoryTraceStore>,
    grant: ExecutionGrant,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let workspace = Arc::new(Workspace::create_temporary().unwrap());
        let root = workspace.host_path().to_owned();
        let grant = ExecutionGrant::new_scoped(
            Principal::new("fixture").unwrap(),
            workspace,
            "Fixture",
            [Capability::ModelInference],
            [ResourceScope {
                kind: ResourceKind::ProviderBinding,
                id: "fixture-target".into(),
            }],
        )
        .unwrap();
        Self {
            engine: TaskEngine::new(MemoryTraceStore::default(), ToolRegistry::default()),
            grant,
            root,
        }
    }
    fn begin(&self) -> Task {
        let task = self.engine.create(&self.grant, "Synthetic task").unwrap();
        let task = self.engine.start(&task.id, &self.grant).unwrap();
        self.engine
            .begin_model_at(
                &task.id,
                task.revision,
                &self.grant,
                "fixture-target",
                "fixture-binding",
                json!({}),
                json!({"model":"fixture"}),
            )
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn step(task: &Task) -> &str {
    let TaskState::ModelThinking { step_id, .. } = &task.state else {
        panic!("reservation")
    };
    step_id
}
fn evidence(text: &str) -> RejectedModelReply {
    RejectedModelReply::capture(
        ModelRejectionReason::InvalidStructuredStep,
        text,
        json!({"model":"fixture"}),
    )
    .unwrap()
}

#[test]
fn rejected_output_is_inert_revision_bound_and_backward_compatible() {
    let f = Fixture::new();
    let reserved = f.begin();
    let raw = r#"```json {"tool_call":{"name":"write_file","arguments":{"path":"never","contents":"never"}}} ```"#;
    assert!(f
        .engine
        .reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            "wrong-step",
            evidence(raw)
        )
        .is_err());
    let rejected = f
        .engine
        .reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            step(&reserved),
            evidence(raw),
        )
        .unwrap();
    assert_eq!(rejected.state, TaskState::Running { in_flight: None });
    assert!(rejected.last_model_reply.is_none());
    assert!(rejected.tool_calls.is_empty());
    assert_eq!(
        rejected.last_model_rejection.as_ref().unwrap().reply.text,
        raw
    );
    assert_eq!(rejected.model_steps, 1);
    assert!(rejected.model_bytes > reserved.model_bytes);
    assert!(matches!(
        f.engine.reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            step(&reserved),
            evidence(raw)
        ),
        Err(Error::Conflict)
    ));
    let events = f.engine.store().events(&reserved.id).unwrap();
    assert!(matches!(
        events.last().unwrap().event,
        TraceEvent::ModelRejected { .. }
    ));
    assert!(!events.iter().any(|e| matches!(
        e.event,
        TraceEvent::ModelFinished { .. } | TraceEvent::ToolStarted { .. }
    )));
    let encoded = serde_json::to_string(&rejected).unwrap();
    assert_eq!(decode_task_record(&encoded).unwrap(), rejected);
    let mut old = serde_json::to_value(rejected).unwrap();
    old.as_object_mut().unwrap().remove("last_model_rejection");
    assert!(decode_task_record(&old.to_string())
        .unwrap()
        .last_model_rejection
        .is_none());
    assert!(!f.root.join("never").exists());
}

#[test]
fn diagnostic_budget_counts_json_escaping_preserves_utf8_and_hashes_full_text() {
    let text = format!("{}tail-🐟", "\0🐟\"".repeat(6000));
    let captured = evidence(&text);
    assert!(captured.truncated);
    assert_eq!(captured.original_bytes, text.len() as u64);
    assert!(text.starts_with(&captured.text));
    assert!(captured.text.len() <= 16 * 1024);
    assert!(serde_json::to_vec(&captured).unwrap().len() <= 20 * 1024);
    let hash = RejectedModelReply::capture(ModelRejectionReason::ResponseLimit, &text, json!({}))
        .unwrap()
        .sha256;
    assert_eq!(captured.sha256, hash);
    assert_ne!(evidence(&captured.text).sha256, hash);
    let mut malformed = captured.clone();
    malformed.truncated = false;
    assert!(malformed.validate().is_err());
    let mut malformed = evidence("small");
    malformed.text = "changed".into();
    assert!(malformed.validate().is_err());
    assert!(RejectedModelReply::capture(
        ModelRejectionReason::ResponseLimit,
        &"x".repeat(2 * 1024 * 1024 + 1),
        json!({})
    )
    .is_err());
    let large = "x".repeat(1024 * 1024 + 1);
    let captured = evidence(&large);
    assert_eq!(captured.original_bytes, large.len() as u64);
    assert!(captured.truncated);
    assert_eq!(captured.text.len(), 16 * 1024);
}

#[test]
fn cancellation_or_another_step_cannot_be_overwritten_by_late_diagnostics() {
    let f = Fixture::new();
    let reserved = f.begin();
    let cancelled = f
        .engine
        .cancel_owned_at(&reserved.id, reserved.revision, f.grant.principal(), "Stop")
        .unwrap();
    assert!(matches!(
        f.engine.reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            step(&reserved),
            evidence("late")
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(
        f.engine
            .get_owned(&reserved.id, f.grant.principal())
            .unwrap(),
        cancelled
    );
    assert!(cancelled.last_model_rejection.is_none());
}

#[test]
fn diagnostic_persistence_failure_preserves_the_pending_reservation() {
    #[derive(Clone)]
    struct Failing(MemoryTraceStore);
    impl TraceStore for Failing {
        fn load(&self, id: &str) -> Result<Task> {
            self.0.load(id)
        }
        fn events(&self, id: &str) -> Result<Vec<TraceRecord>> {
            self.0.events(id)
        }
        fn commit(&self, expected: Option<u64>, task: &Task, event: &TraceEvent) -> Result<()> {
            if matches!(event, TraceEvent::ModelRejected { .. }) {
                return Err(Error::Persistence("synthetic refused write".into()));
            }
            self.0.commit(expected, task, event)
        }
    }
    let f = Fixture::new();
    let reserved = f.begin();
    let engine = TaskEngine::new(Failing(f.engine.store().clone()), ToolRegistry::default());
    assert!(matches!(
        engine.reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            step(&reserved),
            evidence("invalid")
        ),
        Err(Error::Persistence(_))
    ));
    assert_eq!(
        engine.get_owned(&reserved.id, f.grant.principal()).unwrap(),
        reserved
    );
}

#[test]
fn reserved_response_budget_can_record_refusal_without_allowing_another_step() {
    let f = Fixture::new();
    let initial = f.engine.create(&f.grant, "At the byte limit").unwrap();
    let mut running = f.engine.start(&initial.id, &f.grant).unwrap();
    let prior = running.revision;
    running.revision += 1;
    running.model_bytes = 4 * 1024 * 1024 - 20 * 1024 - 2;
    f.engine
        .store()
        .commit(
            Some(prior),
            &running,
            &TraceEvent::PlanUpdated { steps: vec![] },
        )
        .unwrap();
    let reserved = f
        .engine
        .begin_model_at(
            &running.id,
            running.revision,
            &f.grant,
            "fixture-target",
            "fixture-binding",
            json!({}),
            json!({}),
        )
        .unwrap();
    let record = evidence(&"\0".repeat(16000));
    let rejected = f
        .engine
        .reject_model_at(
            &reserved.id,
            reserved.revision,
            f.grant.principal(),
            step(&reserved),
            record,
        )
        .unwrap();
    assert!(rejected.model_bytes <= 4 * 1024 * 1024);
    assert!(matches!(
        f.engine.begin_model_at(
            &rejected.id,
            rejected.revision,
            &f.grant,
            "fixture-target",
            "fixture-binding",
            json!({}),
            json!({})
        ),
        Err(Error::LimitExceeded(_))
    ));
}
