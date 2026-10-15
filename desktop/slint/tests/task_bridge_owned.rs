#![cfg(unix)]
//! Exact normal-backend journey; never discovers or adopts an existing service.
use smith_native_bridge::{
    bridge::{Bridge, Transport},
    owned_backend::OwnedBackend,
    task_bridge::*,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct ModelFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl ModelFixture {
    fn launch() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = requests.clone();
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("owned synthetic listener: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut head = Vec::new();
                let mut byte = [0];
                while head.len() < 8192 && !head.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                }
                assert!(head.ends_with(b"\r\n\r\n"));
                let head = String::from_utf8(head).unwrap();
                let request = head.lines().next().unwrap().to_owned();
                recorded.lock().unwrap().push(request.clone());
                assert!(recorded.lock().unwrap().len() <= 16);
                assert_eq!(request, "GET /v1/models HTTP/1.1");
                let body = r#"{"data":[{"id":"native-synthetic-local"}]}"#;
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for ModelFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

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
fn act<T: Transport>(
    bridge: &mut Bridge<T>,
    snapshot: &TaskSnapshot,
    action: TaskAction,
) -> TaskSnapshot {
    let reviewed = bridge.task_review(snapshot).unwrap();
    let action = bridge
        .task_action_review(&reviewed, &new_task_request_id().unwrap(), action)
        .unwrap();
    bridge.task_action(&action).unwrap()
}
fn approve<T: Transport>(bridge: &mut Bridge<T>, snapshot: &TaskSnapshot) -> TaskSnapshot {
    let review = bridge
        .task_approval_review(&bridge.task_review(snapshot).unwrap())
        .unwrap();
    bridge
        .task_decide(&review, ApprovalDecision::Approve, true)
        .unwrap()
}
fn stopped(backend: &OwnedBackend) {
    let pid = backend.pid();
    backend.stop().unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
#[test]
#[ignore = "requires explicitly pinned normal backend and owned loopback execution"]
fn actual_workspace_task_approval_clarification_cancel_trace_and_restart() {
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").expect("explicit reviewed backend required"),
    );
    let unique = new_task_request_id().unwrap();
    let profile = std::env::temp_dir().join(format!("smith-task-native-owned-{unique}"));
    std::fs::create_dir(&profile).unwrap();
    let owned = OwnedFiles {
        profile: profile.canonicalize().unwrap(),
        marker: format!("native-task-review-{unique}.txt"),
        contents: format!("owned synthetic marker {unique}"),
    };
    let backend = OwnedBackend::launch(&executable, &owned.profile).unwrap();
    let mut bridge = Bridge::new(backend.transport());
    bridge.initialize().unwrap();
    assert!(bridge.task_overview().unwrap().workspaces.is_empty());
    assert!(bridge
        .task_local_model(&bridge.view.config.clone())
        .is_err());
    let provision = bridge
        .task_provision_review(
            WorkspaceProposal {
                label: "Native synthetic task workspace".into(),
                capabilities: vec![
                    Permission::Read,
                    Permission::List,
                    Permission::Write,
                    Permission::Search,
                    Permission::Edit,
                    Permission::Control,
                ],
                resources: vec![],
            },
            None,
        )
        .unwrap();
    let workspace = bridge.task_provision(&provision, true).unwrap();
    let workspace_review = bridge.task_workspace_review(&workspace).unwrap();
    let pending = bridge
        .task_create(&workspace_review, "Synthetic native task journey")
        .unwrap();
    assert_eq!(pending.task.state, TaskState::Pending);
    let start = bridge.task_review(&pending).unwrap();
    let running = bridge.task_start(&start).unwrap();
    assert!(bridge.task_start(&start).is_err()); // stale displayed revision, no replay
    let write = act(
        &mut bridge,
        &running,
        TaskAction::Write {
            path: owned.marker.clone(),
            contents: owned.contents.clone(),
        },
    );
    assert!(matches!(write.task.state, TaskState::NeedsApproval { .. }));
    let approval = bridge
        .task_approval_review(&bridge.task_review(&write).unwrap())
        .unwrap();
    assert!(bridge
        .task_decide(&approval, ApprovalDecision::Approve, false)
        .is_err());
    let running = approve(&mut bridge, &write);
    assert!(!running.operation_failed(), "{:?}", running.outcome);
    assert!(bridge
        .task_decide(&approval, ApprovalDecision::Approve, true)
        .is_err());
    let read = act(
        &mut bridge,
        &running,
        TaskAction::Read {
            path: owned.marker.clone(),
        },
    );
    assert!(!read.operation_failed(), "{:?}", read.outcome);
    assert!(serde_json::to_string(&read.task.last_result)
        .unwrap()
        .contains(&owned.contents));
    let clarification = act(
        &mut bridge,
        &read,
        TaskAction::Clarify {
            question: "Keep this owned fixture?".into(),
        },
    );
    assert!(matches!(
        clarification.task.state,
        TaskState::NeedsClarification { .. }
    ));
    let running = bridge
        .task_resume(
            &bridge.task_review(&clarification).unwrap(),
            ResumeDecision::Clarification("Yes, for this test only.".into()),
            true,
        )
        .unwrap();
    assert_eq!(
        running.task.clarification_answers,
        ["Yes, for this test only."]
    );
    let durable = act(
        &mut bridge,
        &running,
        TaskAction::Write {
            path: "after-restart.txt".into(),
            contents: "Only after a fresh explicit approval".into(),
        },
    );
    let old_approval = bridge
        .task_approval_review(&bridge.task_review(&durable).unwrap())
        .unwrap();
    let task_id = durable.task.id.clone();
    let first_page = bridge.task_trace(&task_id, None, 2).unwrap();
    assert_eq!(first_page.events.len(), 2);
    let next_page = bridge
        .task_trace(&task_id, first_page.next_after, 100)
        .unwrap();
    assert!(next_page
        .events
        .iter()
        .all(|e| e.revision > first_page.next_after.unwrap()));
    stopped(&backend);
    drop(bridge);
    drop(backend);

    let backend = OwnedBackend::launch(&executable, &owned.profile).unwrap();
    let mut bridge = Bridge::new(backend.transport());
    bridge.initialize().unwrap();
    assert!(bridge
        .task_decide(&old_approval, ApprovalDecision::Approve, true)
        .is_err());
    let reopened = bridge.task_snapshot(&task_id).unwrap();
    assert_eq!(reopened.task.state, durable.task.state);
    assert!(reopened.workspace.available);
    let running = approve(&mut bridge, &reopened);
    assert!(!running.operation_failed(), "{:?}", running.outcome);
    let read = act(
        &mut bridge,
        &running,
        TaskAction::Read {
            path: "after-restart.txt".into(),
        },
    );
    assert!(serde_json::to_string(&read.task.last_result)
        .unwrap()
        .contains("Only after a fresh explicit approval"));
    let cancelled = bridge
        .task_cancel(
            &bridge.task_review(&read).unwrap(),
            "Synthetic journey complete",
        )
        .unwrap();
    assert!(matches!(cancelled.task.state, TaskState::Cancelled { .. }));
    let overview = bridge.task_overview().unwrap();
    assert_eq!(overview.workspaces.len(), 1);
    assert_eq!(overview.tasks.len(), 1);
    assert_eq!(overview.tasks[0].id, task_id);

    // A freshly owned synthetic model-discovery fixture proves the positive
    // local-target protocol, without inference or an existing service. Model
    // review itself must make zero provider requests and grant nothing.
    let fixture = ModelFixture::launch();
    bridge
        .save_connection(
            &bridge.view.config.clone(),
            "openai".into(),
            fixture.endpoint.clone(),
        )
        .unwrap();
    bridge.test_connection(&bridge.view.config.clone()).unwrap();
    bridge
        .select_model(&bridge.view.config.clone(), "native-synthetic-local")
        .unwrap();
    let before = fixture.requests.lock().unwrap().len();
    let model = bridge
        .task_local_model(&bridge.view.config.clone())
        .unwrap();
    assert_eq!(model.provenance()["model"], "native-synthetic-local");
    assert_eq!(fixture.requests.lock().unwrap().len(), before);
    let proposed = bridge
        .task_provision_review(
            WorkspaceProposal {
                label: "Uncommitted local-model grant".into(),
                capabilities: vec![Permission::ModelInference],
                resources: vec![model.resource().clone()],
            },
            Some(&model),
        )
        .unwrap();
    bridge
        .save_connection(&bridge.view.config.clone(), "mock".into(), "".into())
        .unwrap();
    assert!(bridge.task_provision(&proposed, true).is_err());
    assert_eq!(bridge.task_overview().unwrap().workspaces.len(), 1);
    assert!(before > 0 && before <= 16);
    assert!(Path::new(&owned.profile)
        .join("local-workspace/profile.sqlite3")
        .is_file());
    stopped(&backend);
    drop(fixture);
}
