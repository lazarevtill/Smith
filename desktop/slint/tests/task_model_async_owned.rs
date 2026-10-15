#![cfg(unix)]
#[path = "support/task_model_fixture.rs"]
mod fixture;
use fixture::{Step, TaskModelFixture, MODEL};
use serde_json::json;
use smith_native_bridge::{bridge::Bridge, owned_backend::OwnedBackend, task_bridge::*};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
struct OwnedFiles {
    profile: PathBuf,
    marker: String,
    contents: String,
}
impl Drop for OwnedFiles {
    fn drop(&mut self) {
        // Agent workspaces deliberately survive backend exit. Match only the
        // unique direct-child sentinel created by this test, never another
        // workspace's files or a shared profile. The backend fixes TMPDIR=/tmp.
        if let Ok(entries) = std::fs::read_dir("/tmp") {
            for entry in entries.take(4096).flatten() {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("smith-agent-")
                {
                    continue;
                }
                let path = entry.path();
                if !std::fs::symlink_metadata(&path)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                {
                    continue;
                }
                let marker = path.join(&self.marker);
                if std::fs::symlink_metadata(&marker).is_ok_and(|m| {
                    m.is_file()
                        && !m.file_type().is_symlink()
                        && m.len() == self.contents.len() as u64
                }) && std::fs::read_to_string(&marker).is_ok_and(|s| s == self.contents)
                {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn reap(backend: &OwnedBackend) {
    let pid = backend.pid();
    backend.stop().unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
#[ignore = "requires explicitly hash-checked async backend and owned loopback fixture"]
fn owned_async_submit_returns_while_model_is_held_then_polls_approves_and_cancels() {
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit reviewed backend required"),
    );
    let unique = new_task_request_id().unwrap();
    let profile = std::env::temp_dir().join(format!("smith-native-async-task-{unique}"));
    std::fs::create_dir(&profile).unwrap();
    let owned = OwnedFiles {
        profile: profile.canonicalize().unwrap(),
        marker: format!("native-async-{unique}.txt"),
        contents: format!("owned asynchronous fixture {unique}"),
    };
    let model = TaskModelFixture::launch(vec![
        Step::tool(
            "write_file",
            json!({"path":owned.marker,"contents":owned.contents}),
            true,
        ),
        Step::tool(
            "write_file",
            json!({"path":"never.txt","contents":"late unapproved proposal"}),
            true,
        ),
    ])
    .unwrap();
    let backend = OwnedBackend::launch(&executable, &owned.profile).unwrap();
    let mut bridge = Bridge::new(backend.transport());
    bridge.initialize().unwrap();
    bridge
        .save_connection(
            &bridge.view.config.clone(),
            "openai".into(),
            model.endpoint.clone(),
        )
        .unwrap();
    bridge.test_connection(&bridge.view.config.clone()).unwrap();
    bridge
        .select_model(&bridge.view.config.clone(), MODEL)
        .unwrap();
    let selected = bridge
        .task_local_model(&bridge.view.config.clone())
        .unwrap();
    let provision = bridge
        .task_provision_review(
            WorkspaceProposal {
                label: "Native asynchronous task".into(),
                capabilities: vec![
                    Permission::ModelInference,
                    Permission::Write,
                    Permission::Read,
                    Permission::List,
                ],
                resources: vec![selected.resource().clone()],
            },
            Some(&selected),
        )
        .unwrap();
    let workspace = bridge.task_provision(&provision, true).unwrap();
    let task = bridge
        .task_create(
            &bridge.task_workspace_review(&workspace).unwrap(),
            "Write the reviewed synthetic file",
        )
        .unwrap();
    let review = bridge
        .task_model_run_review(&bridge.task_review(&task).unwrap(), &selected, 1)
        .unwrap();
    let started = Instant::now();
    let operation = bridge.task_model_submit(&review, true).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    model.wait_entered(1, Duration::from_secs(3));
    assert_eq!(
        bridge.task_model_poll(&operation).unwrap().phase,
        ModelRunPhase::Running
    );
    assert!(bridge.task_model_submit(&review, true).is_err());
    assert_eq!(model.counts().1, 1);
    model.release();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = bridge.task_model_poll(&operation).unwrap();
        if status.is_terminal() {
            assert_eq!(status.phase, ModelRunPhase::Succeeded);
            assert_eq!(status.stop_reason, "needs_approval");
            assert!(status.worker_drained);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let pending = bridge.task_snapshot(&task.task.id).unwrap();
    assert!(matches!(
        pending.task.state,
        TaskState::NeedsApproval { .. }
    ));
    let approval = bridge
        .task_approval_review(&bridge.task_review(&pending).unwrap())
        .unwrap();
    let running = bridge
        .task_decide(&approval, ApprovalDecision::Approve, true)
        .unwrap();
    assert!(!running.operation_failed());
    let read = bridge
        .task_action_review(
            &bridge.task_review(&running).unwrap(),
            &new_task_request_id().unwrap(),
            TaskAction::Read {
                path: owned.marker.clone(),
            },
        )
        .unwrap();
    let running = bridge.task_action(&read).unwrap();
    assert!(!running.operation_failed());
    assert!(serde_json::to_string(&running.task.last_result)
        .unwrap()
        .contains(&owned.contents));
    let next = bridge
        .task_model_run_review(&bridge.task_review(&running).unwrap(), &selected, 1)
        .unwrap();
    let next = bridge.task_model_submit(&next, true).unwrap();
    model.wait_entered(2, Duration::from_secs(3));
    let thinking = bridge.task_snapshot(&task.task.id).unwrap();
    assert!(matches!(
        thinking.task.state,
        TaskState::ModelThinking { .. }
    ));
    let cancelled = bridge
        .task_model_cancel(
            &bridge.task_review(&thinking).unwrap(),
            &next,
            "Cancel the owned synthetic request",
        )
        .unwrap();
    assert!(matches!(cancelled.task.state, TaskState::Cancelled { .. }));
    let status = bridge.task_model_poll(&next).unwrap();
    assert_eq!(status.phase, ModelRunPhase::Cancelled);
    assert!(status.worker_drained);
    assert!(!status.cleanup_unconfirmed);
    assert_eq!(
        status.cancellation,
        Some(ModelCancellationEvidence::ExternalMayContinue)
    );
    model.release();
    model.assert_healthy();
    assert_eq!(model.counts().1, 2);
    let id = task.task.id;
    reap(&backend);
    drop(bridge);
    drop(backend);
    let backend = OwnedBackend::launch(&executable, &owned.profile).unwrap();
    let mut bridge = Bridge::new(backend.transport());
    bridge.initialize().unwrap();
    let task = bridge.task_snapshot(&id).unwrap();
    assert!(matches!(task.task.state, TaskState::Cancelled { .. }));
    assert!(bridge
        .task_model_operations(&bridge.task_review(&task).unwrap())
        .unwrap()
        .is_empty());
    assert!(bridge.task_model_poll(&next).is_err());
    model.assert_healthy();
    assert_eq!(model.counts().1, 2);
    reap(&backend);
}
