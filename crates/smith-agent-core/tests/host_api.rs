use serde_json::json;
use smith_agent_core::*;
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    path: PathBuf,
    owner: Principal,
    grant: ExecutionGrant,
    engine: TaskEngine<MemoryTraceStore>,
    task: Task,
}
impl Fixture {
    fn new() -> Self {
        let workspace = Arc::new(Workspace::create_temporary().unwrap());
        let path = workspace.host_path().to_path_buf();
        let owner = Principal::new("host-owner").unwrap();
        let grant = ExecutionGrant::new(
            owner.clone(),
            workspace,
            "manual",
            [
                Capability::ReadWorkspace,
                Capability::WriteWorkspace,
                Capability::TaskControl,
            ],
        )
        .unwrap();
        let engine = TaskEngine::new(MemoryTraceStore::default(), ToolRegistry::default());
        let task = engine.create(&grant, "synthetic host workflow").unwrap();
        Self {
            path,
            owner,
            grant,
            engine,
            task,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn revision_bound_host_actions_do_not_execute_against_unreviewed_state() {
    let f = Fixture::new();
    let running = f.engine.start_at(&f.task.id, 0, &f.grant).unwrap();
    assert_eq!(
        f.engine.start_at(&f.task.id, 0, &f.grant),
        Err(Error::Conflict)
    );
    let call = ToolCall::new("write_file", json!({"path":"note","contents":"preserve"}));
    assert_eq!(
        f.engine.call_at(&f.task.id, 0, &f.grant, call.clone()),
        Err(Error::Conflict)
    );
    assert_eq!(f.engine.get_owned(&f.task.id, &f.owner).unwrap(), running);
    assert_eq!(
        f.engine
            .call_at(&f.task.id, running.revision, &f.grant, call)
            .unwrap(),
        ToolOutcome::NeedsApproval
    );
    assert!(std::fs::read_dir(&f.path).unwrap().next().is_none());
}

#[test]
fn owner_can_inspect_and_cancel_when_temporary_workspace_is_gone() {
    let f = Fixture::new();
    std::fs::remove_dir(&f.path).unwrap();
    assert!(Workspace::reopen_temporary(&f.task.grant.workspace).is_err());
    let task = f.engine.get_owned(&f.task.id, &f.owner).unwrap();
    let cancelled = f
        .engine
        .cancel_owned_at(&task.id, task.revision, &f.owner, "Workspace expired")
        .unwrap();
    assert!(matches!(cancelled.state, TaskState::Cancelled { .. }));
    assert!(!f.path.exists());
    assert!(matches!(
        f.engine
            .get_owned(&task.id, &Principal::new("another-owner").unwrap()),
        Err(Error::Denied(_))
    ));
}

#[test]
fn owner_recovery_without_workspace_records_uncertainty_and_never_replays() {
    let f = Fixture::new();
    let task = f.engine.start_at(&f.task.id, 0, &f.grant).unwrap();
    let call = ToolCall::new("write_file", json!({"path":"never","contents":"never"}));
    let mut in_flight = task.clone();
    in_flight.revision += 1;
    in_flight.state = TaskState::Running {
        in_flight: Some(call.clone()),
    };
    f.engine
        .store()
        .commit(
            Some(task.revision),
            &in_flight,
            &TraceEvent::ToolStarted {
                call,
                approved: true,
            },
        )
        .unwrap();
    std::fs::remove_dir(&f.path).unwrap();
    let interrupted = f
        .engine
        .mark_interrupted_owned_at(&task.id, in_flight.revision, &f.owner)
        .unwrap();
    assert!(matches!(
        interrupted.state,
        TaskState::Interrupted {
            uncertain_call: Some(_)
        }
    ));
    let resumed = f
        .engine
        .continue_owned_at(&task.id, interrupted.revision, &f.owner)
        .unwrap();
    assert_eq!(resumed.state, TaskState::Running { in_flight: None });
    assert!(!f.path.exists());
    assert_eq!(
        f.engine
            .cancel_owned_at(&task.id, interrupted.revision, &f.owner, "stale cancel"),
        Err(Error::Conflict)
    );
}

#[test]
fn host_trace_codec_preserves_typed_records_and_checks_revision_and_size() {
    let f = Fixture::new();
    let (snapshot, event) = encode_transition(None, &f.task, &TraceEvent::Created).unwrap();
    assert_eq!(decode_task_record(&snapshot).unwrap(), f.task);
    assert_eq!(decode_trace_event(&event).unwrap(), TraceEvent::Created);
    assert_eq!(
        encode_transition(Some(0), &f.task, &TraceEvent::Created),
        Err(Error::Conflict)
    );
    assert!(matches!(
        decode_task_record(&" ".repeat(16 * 1024 * 1024 + 1)),
        Err(Error::LimitExceeded(_))
    ));
}
