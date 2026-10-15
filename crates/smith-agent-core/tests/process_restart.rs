#![cfg(unix)]
use serde::{Deserialize, Serialize};
use serde_json::json;
use smith_agent_core::*;
use std::{path::Path, process::Command, sync::Arc};

#[derive(Serialize, Deserialize)]
struct Handoff {
    id: String,
    workspace: WorkspaceIdentity,
}

#[test]
fn approval_and_clarification_survive_three_cold_processes() {
    let fixture = tempfile::tempdir().unwrap();
    for phase in ["seed", "approve_and_pause", "resume_and_finish"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cold_process_child", "--nocapture"])
            .env("SMITH_CORE_FIXTURE_DIR", fixture.path())
            .env("SMITH_CORE_FIXTURE_PHASE", phase)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "phase {phase}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let handoff: Handoff =
        serde_json::from_slice(&std::fs::read(fixture.path().join("handoff.json")).unwrap())
            .unwrap();
    let store = SqliteTraceStore::open(fixture.path().join("trace.sqlite")).unwrap();
    let task = store.load(&handoff.id).unwrap();
    assert!(matches!(task.state, TaskState::Completed { .. }));
    let trace = store.events(&handoff.id).unwrap();
    assert_eq!(trace.iter().filter(|r| matches!(&r.event, TraceEvent::ToolStarted { call, .. } if call.name == "write_file")).count(), 1);
    assert_eq!(
        trace
            .iter()
            .filter(|r| matches!(r.event, TraceEvent::ClarificationAnswered { .. }))
            .count(),
        1
    );
    std::fs::remove_dir_all(std::env::temp_dir().join(handoff.workspace.name)).unwrap();
}

// Spawned only by the test above; ordinary discovery returns immediately.
// This is test-harness process creation, never a model-accessible shell tool.
#[test]
fn cold_process_child() {
    let Ok(directory) = std::env::var("SMITH_CORE_FIXTURE_DIR") else {
        return;
    };
    let directory = Path::new(&directory);
    let phase = std::env::var("SMITH_CORE_FIXTURE_PHASE").unwrap();
    if phase == "seed" {
        let workspace = Arc::new(Workspace::create_temporary().unwrap());
        let grant = explicit_fixture_grant(workspace.clone());
        let engine = TaskEngine::new(
            SqliteTraceStore::open(directory.join("trace.sqlite")).unwrap(),
            ToolRegistry::default(),
        );
        let task = engine.create(&grant, "Cold process fixture").unwrap();
        engine.start(&task.id, &grant).unwrap();
        assert_eq!(
            engine
                .call(
                    &task.id,
                    &grant,
                    ToolCall::new(
                        "write_file",
                        json!({"path":"cold.txt","contents":"cold-process data"})
                    )
                )
                .unwrap(),
            ToolOutcome::NeedsApproval
        );
        assert!(!workspace.host_path().join("cold.txt").exists());
        let handoff = Handoff {
            id: task.id,
            workspace: workspace.identity().clone(),
        };
        std::fs::write(
            directory.join("handoff.json"),
            serde_json::to_vec(&handoff).unwrap(),
        )
        .unwrap();
        return;
    }
    let handoff: Handoff =
        serde_json::from_slice(&std::fs::read(directory.join("handoff.json")).unwrap()).unwrap();
    let workspace = Arc::new(Workspace::reopen_temporary(&handoff.workspace).unwrap());
    let grant = explicit_fixture_grant(workspace);
    let engine = TaskEngine::new(
        SqliteTraceStore::open(directory.join("trace.sqlite")).unwrap(),
        ToolRegistry::default(),
    );
    let task = engine.get(&handoff.id, &grant).unwrap();
    if phase == "approve_and_pause" {
        assert!(matches!(task.state, TaskState::NeedsApproval { .. }));
        engine
            .decide_approval(&task.id, task.revision, &grant, ApprovalDecision::Approve)
            .unwrap();
        let read = engine
            .call(
                &task.id,
                &grant,
                ToolCall::new("read_file", json!({"path":"cold.txt"})),
            )
            .unwrap();
        let ToolOutcome::Returned(read) = read else {
            panic!("read failed")
        };
        assert_eq!(read["contents"], "cold-process data");
        assert_eq!(read["path"], "cold.txt");
        assert_eq!(read["sha256"].as_str().unwrap().len(), 64);
        engine
            .call(
                &task.id,
                &grant,
                ToolCall::new("ask_clarification", json!({"question":"Ready to finish?"})),
            )
            .unwrap();
    } else {
        assert_eq!(phase, "resume_and_finish");
        assert!(matches!(task.state, TaskState::NeedsClarification { .. }));
        engine
            .resume_clarification(&task.id, task.revision, &grant, "Yes")
            .unwrap();
        engine
            .call(
                &task.id,
                &grant,
                ToolCall::new(
                    "task_complete",
                    json!({"summary":"Cold process restart passed"}),
                ),
            )
            .unwrap();
    }
}

fn explicit_fixture_grant(workspace: Arc<Workspace>) -> ExecutionGrant {
    ExecutionGrant::new(
        Principal::new("cold-process-fixture").unwrap(),
        workspace,
        "agent",
        [
            Capability::WriteWorkspace,
            Capability::ReadWorkspace,
            Capability::TaskControl,
        ],
    )
    .unwrap()
}
