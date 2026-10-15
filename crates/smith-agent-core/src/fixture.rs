//! Deterministic, offline agent-driver fixture. It exercises real filesystem
//! tools and disk reopen; it is not a model provider or a simulated tool result.
use crate::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Serialize)]
pub struct FixtureReport {
    pub fixture: String,
    pub inference_calls: usize,
    pub task: Task,
    pub trace: Vec<TraceRecord>,
    pub file_contents: String,
    pub search_result: Value,
    pub edit_result: Value,
    pub edited_file_contents: String,
    pub cancelled_task: Task,
    pub implemented_tool_count: usize,
    pub source_tool_count: usize,
    pub restart_verified: bool,
}

/// Executes only against a newly created temporary workspace and trace store.
/// Explicit approvals below are fixture inputs, never inferred from text.
pub fn run_deterministic_fixture() -> Result<FixtureReport> {
    let workspace = Arc::new(Workspace::create_temporary()?);
    let _cleanup = Cleanup(workspace.host_path().to_path_buf());
    let identity = workspace.identity().clone();
    let trace_dir = tempfile::tempdir()?;
    let trace_path = trace_dir.path().join("tasks.sqlite");
    let grant = fixture_grant(workspace.clone())?;
    let engine = TaskEngine::new(
        SqliteTraceStore::open(&trace_path)?,
        ToolRegistry::default(),
    );
    let task = engine.create(
        &grant,
        "Create fixture.txt, search and edit it, ask for clarification, then complete.",
    )?;
    let id = task.id;
    engine.start(&id, &grant)?;
    engine.call(
        &id,
        &grant,
        ToolCall::new(
            "update_plan",
            json!({"steps":["Write fixture file","Read, search and edit it","Clarify and complete"]}),
        ),
    )?;
    let outcome = engine.call(
        &id,
        &grant,
        ToolCall::new(
            "write_file",
            json!({"path":"fixture.txt","contents":"A real, confined filesystem result.\n"}),
        ),
    )?;
    if outcome != ToolOutcome::NeedsApproval {
        return Err(Error::InvalidTransition(
            "fixture did not pause for approval".into(),
        ));
    }
    drop(engine);
    drop(grant);
    drop(workspace);

    // Explicitly reconstruct authority from trusted fixture configuration,
    // checking directory identity. The persisted task cannot grant it itself.
    let grant = fixture_grant(Arc::new(Workspace::reopen_temporary(&identity)?))?;
    let engine = TaskEngine::new(
        SqliteTraceStore::open(&trace_path)?,
        ToolRegistry::default(),
    );
    let waiting = engine.get(&id, &grant)?;
    engine.decide_approval(&id, waiting.revision, &grant, ApprovalDecision::Approve)?;
    engine.call(&id, &grant, ToolCall::new("list_directory", json!({})))?;
    let read = engine.call(
        &id,
        &grant,
        ToolCall::new("read_file", json!({"path":"fixture.txt"})),
    )?;
    let ToolOutcome::Returned(value) = read else {
        return Err(Error::Io("fixture file read failed".into()));
    };
    let file_contents = value["contents"]
        .as_str()
        .ok_or_else(|| Error::Io("fixture missing content".into()))?
        .into();
    let ToolOutcome::Returned(search_result) = engine.call(
        &id,
        &grant,
        ToolCall::new("search", json!({"query":"confined"})),
    )?
    else {
        return Err(Error::Io("fixture search failed".into()));
    };
    let outcome = engine.call(
        &id,
        &grant,
        ToolCall::new("edit_file", json!({"path":"fixture.txt","old_string":"confined","new_string":"guarded","expected_sha256":value["sha256"]})),
    )?;
    if outcome != ToolOutcome::NeedsApproval {
        return Err(Error::InvalidTransition(
            "fixture edit did not pause for approval".into(),
        ));
    }
    // The edit's exact request and expected file version survive a restart too.
    drop(engine);
    let engine = TaskEngine::new(
        SqliteTraceStore::open(&trace_path)?,
        ToolRegistry::default(),
    );
    let waiting = engine.get(&id, &grant)?;
    let ToolOutcome::Returned(edit_result) =
        engine.decide_approval(&id, waiting.revision, &grant, ApprovalDecision::Approve)?
    else {
        return Err(Error::Io("fixture edit failed".into()));
    };
    let ToolOutcome::Returned(edited) = engine.call(
        &id,
        &grant,
        ToolCall::new("read_file", json!({"path":"fixture.txt"})),
    )?
    else {
        return Err(Error::Io("fixture edited file read failed".into()));
    };
    let edited_file_contents = edited["contents"]
        .as_str()
        .ok_or_else(|| Error::Io("fixture missing edited content".into()))?
        .into();
    engine.call(
        &id,
        &grant,
        ToolCall::new(
            "ask_clarification",
            json!({"question":"Finish this fixture?"}),
        ),
    )?;
    drop(engine);

    let engine = TaskEngine::new(
        SqliteTraceStore::open(&trace_path)?,
        ToolRegistry::default(),
    );
    let paused = engine.get(&id, &grant)?;
    engine.resume_clarification(&id, paused.revision, &grant, "Yes, finish.")?;
    engine.call(&id, &grant, ToolCall::new("task_complete", json!({"summary":"Created, searched and edited fixture.txt with explicit approvals and durable clarification resume."})))?;

    let cancelled = engine.create(&grant, "Cancellation fixture")?;
    engine.start(&cancelled.id, &grant)?;
    engine.call(
        &cancelled.id,
        &grant,
        ToolCall::new(
            "write_file",
            json!({"path":"cancelled.txt","contents":"must never execute"}),
        ),
    )?;
    let cancelled_task = engine.cancel(&cancelled.id, &grant, "User cancelled before approval")?;
    let tools = engine.registry().all_tools();
    Ok(FixtureReport {
        fixture: "deterministic-agent-v2".into(),
        inference_calls: 0,
        task: engine.get(&id, &grant)?,
        trace: engine.store().events(&id)?,
        file_contents,
        search_result,
        edit_result,
        edited_file_contents,
        cancelled_task,
        implemented_tool_count: tools
            .iter()
            .filter(|t| t.availability == Availability::Implemented)
            .count(),
        source_tool_count: tools.len(),
        restart_verified: true,
    })
}

fn fixture_grant(workspace: Arc<Workspace>) -> Result<ExecutionGrant> {
    ExecutionGrant::new(
        Principal::new("offline-fixture")?,
        workspace,
        "fixture-agent",
        [
            Capability::ReadWorkspace,
            Capability::ListWorkspace,
            Capability::WriteWorkspace,
            Capability::SearchWorkspace,
            Capability::EditWorkspace,
            Capability::TaskControl,
        ],
    )
}
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
