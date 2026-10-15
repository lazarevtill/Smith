use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Config, Result, Transport},
    task_bridge::*,
};
use std::sync::{Arc, Mutex};

const WS: &str = "11111111-1111-4111-8111-111111111111";
const TASK: &str = "22222222-2222-4222-8222-222222222222";
const CALL: &str = "33333333-3333-4333-8333-333333333333";
const OTHER: &str = "44444444-4444-4444-8444-444444444444";
fn workspace() -> WorkspaceSnapshot {
    serde_json::from_value(json!({"id":WS,"label":"Synthetic","capabilities":["read","write","control"],"resources":[],"policy_revision":1,"available":true,"output_boundary":"plaintext_private_temporary_files"})).unwrap()
}
fn snapshot(state: TaskState, revision: u64) -> TaskSnapshot {
    TaskSnapshot {
        task: TaskRecord {
            id: TASK.into(),
            revision,
            workspace_id: WS.into(),
            input: "Synthetic task".into(),
            state,
            plan: vec![],
            clarification_answers: vec![],
            last_result: None,
            model_steps: 0,
            model_bytes: 0,
            last_model_reply: None,
            capabilities: None,
            resources: None,
            agent_persona: None,
            extensions: None,
        },
        workspace: workspace(),
        outcome: None,
    }
}
fn call(name: &str) -> ToolCall {
    ToolCall {
        id: CALL.into(),
        name: name.into(),
        arguments: json!({"path":"owned.txt","contents":"reviewed"}),
    }
}
fn config() -> Config {
    Config {
        revision: "profile:1".into(),
        provider: "mock".into(),
        endpoint: "".into(),
        model: "smith-demo".into(),
        tested: true,
    }
}
#[derive(Default)]
struct State {
    requests: Vec<(String, String, Option<Value>)>,
    response: Option<Value>,
    responses: std::collections::VecDeque<Value>,
    failure: bool,
    async_failure: bool,
    revoke_on_async: bool,
    revoke_on_mutation: bool,
    revoked: bool,
}
struct Mock(Arc<Mutex<State>>);
impl Transport for Mock {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut s = self.0.lock().unwrap();
        s.requests.push((method.into(), path.into(), body));
        if path == "/api/profile/status" {
            return Ok(
                json!({"state":"unlocked","session":if s.revoked {"next-profile"}else{"first-profile"}}),
            );
        }
        assert_eq!(session, Some("first-profile"));
        if path == "/api/config" {
            return Ok(serde_json::to_value(config()).unwrap());
        }
        if path.starts_with("/api/models?") {
            return Ok(json!({"revision":"profile:1","models":[{"id":"smith-demo"}]}));
        }
        if path == "/api/chats" {
            return Ok(json!([]));
        }
        if path == "/api/media/artifacts" {
            return Ok(json!({"artifacts":[]}));
        }
        if path == "/api/actions" {
            return Ok(json!({"revision":0,"undo":null,"redo":null}));
        }
        if path == "/api/trash" {
            return Ok(json!({"chats":[]}));
        }
        if path == "/api/drafts/new" {
            return Ok(json!({"owner":"new","version":0,"text":"","attachments":[],"media":[]}));
        }
        if method != "GET" && s.revoke_on_mutation {
            s.revoked = true;
        }
        if method == "POST" && path.ends_with("/model-runs") {
            if s.revoke_on_async {
                s.revoked = true;
            }
            if s.async_failure {
                return Err("whole request deadline".into());
            }
        }
        if method != "GET" && s.failure {
            return Err("whole request deadline".into());
        }
        if let Some(value) = s.responses.pop_front() {
            return Ok(value);
        }
        if let Some(value) = s.response.clone() {
            return Ok(value);
        }
        if path == "/api/agent/tasks" {
            return Ok(json!({"tasks":[]}));
        }
        if path == "/api/agent/workspaces" {
            return Ok(json!({"workspaces":[workspace()]}));
        }
        panic!("unexpected owned fixture request {method} {path}");
    }
}
fn make_bridge() -> (Bridge<Mock>, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State::default()));
    let mut bridge = Bridge::new(Mock(state.clone()));
    bridge.initialize().unwrap();
    state.lock().unwrap().requests.clear();
    (bridge, state)
}
fn mutations(state: &Arc<Mutex<State>>) -> Vec<(String, String, Option<Value>)> {
    state
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.0 != "GET")
        .cloned()
        .collect()
}

#[test]
fn provision_requires_exact_permissions_and_confirmation() {
    let (mut bridge, state) = make_bridge();
    let proposal = WorkspaceProposal {
        label: "Synthetic".into(),
        capabilities: vec![Permission::Read, Permission::Write, Permission::Control],
        resources: vec![],
    };
    let review = bridge.task_provision_review(proposal, None).unwrap();
    assert!(bridge.task_provision(&review, false).is_err());
    assert!(mutations(&state).is_empty());
    state.lock().unwrap().response = Some(json!({"workspace":workspace()}));
    assert_eq!(bridge.task_provision(&review, true).unwrap(), workspace());
    assert_eq!(
        mutations(&state)[0].2.as_ref().unwrap()["capabilities"],
        json!(["read", "write", "control"])
    );
    let mut unsafe_proposal = review.proposal().clone();
    unsafe_proposal.capabilities.push(Permission::Shell);
    assert!(bridge.task_provision_review(unsafe_proposal, None).is_err());
    let mut duplicated = review.proposal().clone();
    duplicated.capabilities.push(Permission::Read);
    assert!(bridge.task_provision_review(duplicated, None).is_err());
}

#[test]
fn action_and_approval_capture_exact_revision_identity_and_arguments() {
    let (mut bridge, state) = make_bridge();
    let running = bridge
        .task_review(&snapshot(TaskState::Running { in_flight: None }, 3))
        .unwrap();
    let action = bridge
        .task_action_review(
            &running,
            CALL,
            TaskAction::Write {
                path: "owned.txt".into(),
                contents: "reviewed".into(),
            },
        )
        .unwrap();
    let pending = snapshot(
        TaskState::NeedsApproval {
            call: call("write_file"),
        },
        4,
    );
    state.lock().unwrap().response = Some(serde_json::to_value(&pending).unwrap());
    let result = bridge.task_action(&action).unwrap();
    let captured = bridge
        .task_approval_review(&bridge.task_review(&result).unwrap())
        .unwrap();
    let mut changed = result.clone();
    changed.task.state = TaskState::NeedsApproval {
        call: call("run_shell"),
    };
    assert_eq!(captured.call().name, "write_file");
    state.lock().unwrap().response =
        Some(serde_json::to_value(snapshot(TaskState::Running { in_flight: None }, 6)).unwrap());
    assert!(bridge
        .task_decide(&captured, ApprovalDecision::Approve, false)
        .is_err());
    bridge
        .task_decide(&captured, ApprovalDecision::Approve, true)
        .unwrap();
    let calls = mutations(&state);
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].2.as_ref().unwrap(),
        &json!({"expected_revision":3,"request_id":CALL,"action":{"kind":"write","path":"owned.txt","contents":"reviewed"}})
    );
    assert_eq!(
        calls[1].2.as_ref().unwrap(),
        &json!({"expected_revision":4,"call_id":CALL,"decision":"approve"})
    );
}

#[test]
fn revoked_session_prevents_mutation_and_late_body_clears_bridge() {
    let (mut bridge, state) = make_bridge();
    let review = bridge
        .task_review(&snapshot(TaskState::Pending, 0))
        .unwrap();
    state.lock().unwrap().revoked = true;
    assert!(bridge.task_start(&review).is_err());
    assert!(mutations(&state).is_empty());
    assert!(!bridge.view.available);
    let (mut bridge, state) = make_bridge();
    let review = bridge
        .task_review(&snapshot(TaskState::Pending, 0))
        .unwrap();
    {
        let mut s = state.lock().unwrap();
        s.revoke_on_mutation = true;
        s.response = Some(
            serde_json::to_value(snapshot(TaskState::Running { in_flight: None }, 1)).unwrap(),
        );
    }
    assert!(bridge
        .task_start(&review)
        .unwrap_err()
        .contains("no longer admitted"));
    assert!(!bridge.view.available);
    assert_eq!(mutations(&state).len(), 1);
}

#[test]
fn wrong_task_or_policy_response_is_never_accepted() {
    let (mut bridge, state) = make_bridge();
    let review = bridge
        .task_review(&snapshot(TaskState::Pending, 0))
        .unwrap();
    let mut wrong = snapshot(TaskState::Running { in_flight: None }, 1);
    wrong.task.id = OTHER.into();
    state.lock().unwrap().response = Some(serde_json::to_value(&wrong).unwrap());
    assert!(bridge.task_start(&review).is_err());
    wrong.task.id = TASK.into();
    wrong.workspace.policy_revision = 2;
    state.lock().unwrap().response = Some(serde_json::to_value(wrong).unwrap());
    assert!(bridge.task_start(&review).is_err());
}

#[test]
fn ambiguous_mutation_is_not_replayed_or_reported_as_effect_success() {
    let (mut bridge, state) = make_bridge();
    let review = bridge
        .task_review(&snapshot(TaskState::Pending, 0))
        .unwrap();
    state.lock().unwrap().failure = true;
    assert!(bridge
        .task_start(&review)
        .unwrap_err()
        .contains("may already have been recorded"));
    assert_eq!(mutations(&state).len(), 1);
    let mut failed = snapshot(TaskState::Running { in_flight: None }, 2);
    failed.outcome = Some(json!({"kind":"failed","value":{"code":"conflict"}}));
    assert!(failed.operation_failed());
}

#[test]
fn dedicated_effect_approval_and_retry_cannot_use_generic_route() {
    let (mut bridge, state) = make_bridge();
    let pending = snapshot(
        TaskState::NeedsApproval {
            call: call("send_webhook"),
        },
        2,
    );
    let review = bridge
        .task_approval_review(&bridge.task_review(&pending).unwrap())
        .unwrap();
    assert!(bridge
        .task_decide(&review, ApprovalDecision::Approve, true)
        .is_err());
    assert!(mutations(&state).is_empty());
    let interrupted = bridge
        .task_review(&snapshot(
            TaskState::Interrupted {
                uncertain_call: Some(call("run_shell")),
            },
            3,
        ))
        .unwrap();
    assert!(bridge
        .task_resume(&interrupted, ResumeDecision::RetryUncertainCall, true)
        .is_err());
    state.lock().unwrap().response =
        Some(serde_json::to_value(snapshot(TaskState::Running { in_flight: None }, 4)).unwrap());
    bridge
        .task_resume(&interrupted, ResumeDecision::ContinueWithoutReplay, true)
        .unwrap();
    assert_eq!(mutations(&state).len(), 1);
    assert_eq!(
        mutations(&state)[0].2.as_ref().unwrap()["kind"],
        "continue_without_replay"
    );
}

#[test]
fn trace_owner_order_cursor_and_limit_are_validated() {
    let (mut bridge, state) = make_bridge();
    let page = json!({"task_id":TASK,"revision":3,"events":[{"task_id":TASK,"revision":2,"event":{"kind":"started"}}],"next_after":2});
    state.lock().unwrap().response = Some(page.clone());
    assert_eq!(
        bridge.task_trace(TASK, Some(1), 1).unwrap().next_after,
        Some(2)
    );
    let mut bad = page.clone();
    bad["events"][0]["task_id"] = json!(OTHER);
    state.lock().unwrap().response = Some(bad);
    assert!(bridge.task_trace(TASK, Some(1), 1).is_err());
    let mut bad = page;
    bad["next_after"] = Value::Null;
    state.lock().unwrap().response = Some(bad);
    assert!(bridge.task_trace(TASK, Some(1), 1).is_err());
    assert!(bridge.task_trace(TASK, None, 101).is_err());
}

fn model_result(config: &Config, binding: &str) -> Value {
    let scope = format!("provider:{}", "a".repeat(64));
    let binding = format!("model:{}", binding.repeat(64));
    json!({"scope_id":scope,"binding_id":binding,"provenance":{"scope_id":scope,"binding_id":binding,"provider":config.provider,"endpoint":config.endpoint,"model":config.model,"native":null}})
}
#[test]
fn local_model_review_rejects_remote_and_stale_target_before_grant() {
    let (mut bridge, state) = make_bridge();
    let mut local = Config {
        revision: "profile:1".into(),
        provider: "openai".into(),
        endpoint: "https://example.invalid/v1".into(),
        model: "synthetic".into(),
        tested: true,
    };
    bridge.view.config = local.clone();
    assert!(bridge.task_local_model(&local).is_err());
    assert!(mutations(&state).is_empty());
    for endpoint in [
        "http://localhost:1234/v1",
        "http://127.0.0.1:1234@evil.invalid/v1",
        "http://127.0.0.1:1234/v1?secret=x",
        "http://127.0.0.1:0/v1",
    ] {
        local.endpoint = endpoint.into();
        bridge.view.config = local.clone();
        assert!(bridge.task_local_model(&local).is_err());
    }
    local.endpoint = "http://127.0.0.1:1234/v1".into();
    bridge.view.config = local.clone();
    state.lock().unwrap().response = Some(model_result(&local, "b"));
    let model = bridge.task_local_model(&local).unwrap();
    assert_eq!(
        mutations(&state).last().unwrap().2.as_ref().unwrap(),
        &json!({"revision":local.revision,"provider":local.provider,"endpoint":local.endpoint})
    );
    let proposal = WorkspaceProposal {
        label: "Exact local model".into(),
        capabilities: vec![Permission::ModelInference],
        resources: vec![model.resource().clone()],
    };
    assert!(bridge
        .task_provision_review(proposal.clone(), None)
        .is_err());
    let review = bridge
        .task_provision_review(proposal, Some(&model))
        .unwrap();
    state.lock().unwrap().response = Some(model_result(&local, "c"));
    assert!(bridge
        .task_provision(&review, true)
        .unwrap_err()
        .contains("changed"));
    assert!(mutations(&state)
        .iter()
        .all(|r| r.1 == "/api/agent/model-selection"));
}

#[test]
fn clarification_and_terminal_cancellation_follow_displayed_state() {
    let (mut bridge, state) = make_bridge();
    let needs = bridge
        .task_review(&snapshot(
            TaskState::NeedsClarification {
                question: "Which file?".into(),
            },
            4,
        ))
        .unwrap();
    state.lock().unwrap().response =
        Some(serde_json::to_value(snapshot(TaskState::Running { in_flight: None }, 5)).unwrap());
    assert!(bridge
        .task_resume(
            &needs,
            ResumeDecision::Clarification("owned.txt".into()),
            false
        )
        .is_err());
    bridge
        .task_resume(
            &needs,
            ResumeDecision::Clarification("owned.txt".into()),
            true,
        )
        .unwrap();
    let done = bridge
        .task_review(&snapshot(
            TaskState::Cancelled {
                reason: "done".into(),
            },
            8,
        ))
        .unwrap();
    assert!(bridge.task_cancel(&done, "again").is_err());
    assert_eq!(mutations(&state).len(), 1);
}

#[test]
fn invalid_identity_and_large_action_are_rejected_before_dispatch() {
    let (mut bridge, state) = make_bridge();
    assert!(bridge.task_snapshot("../config").is_err());
    let running = bridge
        .task_review(&snapshot(TaskState::Running { in_flight: None }, 3))
        .unwrap();
    assert!(bridge
        .task_action_review(&running, "not-uuid", TaskAction::List)
        .is_err());
    assert!(bridge
        .task_action_review(
            &running,
            CALL,
            TaskAction::Write {
                path: "file".into(),
                contents: "x".repeat(129 * 1024)
            }
        )
        .is_err());
    assert!(bridge
        .task_action_review(
            &running,
            CALL,
            TaskAction::Tool {
                name: "run_shell".into(),
                arguments: json!({})
            }
        )
        .is_err());
    assert!(mutations(&state).is_empty());
    let first = new_task_request_id().unwrap();
    let second = new_task_request_id().unwrap();
    assert_ne!(first, second);
    assert!(bridge
        .task_action_review(&running, &first, TaskAction::List)
        .is_ok());
}

fn local_review(
    bridge: &mut Bridge<Mock>,
    state: &Arc<Mutex<State>>,
) -> (TaskReview, LocalModelReview) {
    let config = Config {
        revision: "profile:1".into(),
        provider: "openai".into(),
        endpoint: "http://127.0.0.1:1234/v1".into(),
        model: "synthetic".into(),
        tested: true,
    };
    bridge.view.config = config.clone();
    state.lock().unwrap().response = Some(model_result(&config, "b"));
    let model = bridge.task_local_model(&config).unwrap();
    let mut task = snapshot(TaskState::Pending, 0);
    task.workspace.capabilities.push(Permission::ModelInference);
    task.workspace.resources.push(model.resource().clone());
    (bridge.task_review(&task).unwrap(), model)
}
fn operation() -> Value {
    json!({"id":CALL,"task_id":TASK,"submitted_revision":0,"max_steps":1,"scope_id":format!("provider:{}","a".repeat(64)),"binding_id":format!("model:{}","b".repeat(64)),"sequence":1,"phase":"running","task_revision":null,"stop_reason":"running","error":null,"worker_drained":false,"cleanup_unconfirmed":false,"cancellation":null})
}
fn operations(values: Vec<Value>) -> Value {
    json!({"task_id":TASK,"operations":values,"retention":"current_profile_session_latest_128","missing_requires_task_inspection":true})
}
#[test]
fn saved_agent_model_review_uses_task_grants_and_captured_target() {
    let (mut bridge, state) = make_bridge();
    let (old_task, model) = local_review(&mut bridge, &state);
    state.lock().unwrap().requests.clear();
    assert!(bridge.task_model_run_review(&old_task, &model, 1).is_ok());
    let mut narrowed = old_task.snapshot().clone();
    narrowed.task.capabilities = Some(vec![]);
    narrowed.task.resources = Some(vec![]);
    let task = bridge.task_review(&narrowed).unwrap();
    assert!(
        bridge.task_model_run_review(&task, &model, 1).is_err(),
        "empty task grants cannot inherit the wider workspace"
    );
    narrowed.task.capabilities = Some(vec![Permission::ModelInference]);
    let task = bridge.task_review(&narrowed).unwrap();
    assert!(
        bridge.task_model_run_review(&task, &model, 1).is_err(),
        "the model resource must also be granted to the task"
    );
    narrowed.task.resources = Some(vec![model.resource().clone()]);
    narrowed.task.agent_persona = Some(AgentPersona {
        definition_id: OTHER.into(),
        definition_revision: 1,
        name: "Synthetic agent".into(),
        system_prompt: String::new(),
        soul: String::new(),
        model_target: Some(AgentModelTarget {
            provider: model.config().provider.clone(),
            endpoint: model.config().endpoint.clone(),
            model: model.config().model.clone(),
            scope_id: model.resource().id.clone(),
        }),
    });
    let task = bridge.task_review(&narrowed).unwrap();
    assert!(bridge.task_model_run_review(&task, &model, 1).is_ok());
    narrowed
        .task
        .agent_persona
        .as_mut()
        .unwrap()
        .model_target
        .as_mut()
        .unwrap()
        .model = "different-model".into();
    let task = bridge.task_review(&narrowed).unwrap();
    assert!(bridge.task_model_run_review(&task, &model, 1).is_err());
    narrowed.task.capabilities = None;
    assert!(
        bridge.task_review(&narrowed).is_err(),
        "partial grants must not fall back"
    );
    narrowed.task.capabilities = Some(vec![Permission::ModelInference]);
    narrowed.task.resources = Some(vec![ResourceScope {
        kind: ResourceKind::ProviderBinding,
        id: "ungranted-model".into(),
    }]);
    assert!(
        bridge.task_review(&narrowed).is_err(),
        "task grants must remain within workspace policy"
    );
    assert!(
        mutations(&state).is_empty(),
        "review must never dispatch inference or any effect"
    );
}
#[test]
fn asynchronous_model_submit_is_exact_and_polling_never_replays_or_changes_owner() {
    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    assert!(bridge.task_model_run_review(&task, &model, 9).is_err());
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    state.lock().unwrap().requests.clear();
    assert!(bridge.task_model_submit(&review, false).is_err());
    assert!(mutations(&state).is_empty());
    state.lock().unwrap().responses = std::collections::VecDeque::from([
        model_result(model.config(), "b"),
        operations(vec![]),
        json!({"operation":operation()}),
    ]);
    let admitted = bridge.task_model_submit(&review, true).unwrap();
    let requests = mutations(&state);
    assert_eq!(requests.len(), 2);
    assert!(requests[1].1.ends_with("/model-runs"));
    assert_eq!(
        requests[1].2.as_ref().unwrap(),
        &json!({"expected_revision":0,"expected":{"revision":"profile:1","provider":"openai","endpoint":"http://127.0.0.1:1234/v1"},"max_steps":1,"confirmed":true,"binding_id":format!("model:{}","b".repeat(64))})
    );
    let mut terminal = operation();
    terminal["phase"] = json!("succeeded");
    terminal["worker_drained"] = json!(true);
    terminal["task_revision"] = json!(3);
    terminal["stop_reason"] = json!("needs_approval");
    state.lock().unwrap().response = Some(json!({"operation":terminal}));
    assert!(bridge.task_model_poll(&admitted).unwrap().is_terminal());
    assert_eq!(mutations(&state).len(), 2);
    let mut wrong = operation();
    wrong["binding_id"] = json!(format!("model:{}", "c".repeat(64)));
    state.lock().unwrap().response = Some(json!({"operation":wrong}));
    assert!(bridge.task_model_poll(&admitted).is_err());
    let other = bridge
        .task_review(&{
            let mut t = task.snapshot().clone();
            t.task.id = OTHER.into();
            t
        })
        .unwrap();
    assert!(bridge
        .task_model_cancel(&other, &admitted, "wrong owner")
        .is_err());
    state.lock().unwrap().revoked = true;
    assert!(bridge.task_model_poll(&admitted).is_err());
    assert!(!bridge.view.available);
}
#[test]
fn lost_async_submit_requires_inspection_and_retained_status_list_is_bounded() {
    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    // The provider review is read-only; fail only the actual async submission.
    state.lock().unwrap().responses =
        std::collections::VecDeque::from([model_result(model.config(), "b"), operations(vec![])]);
    state.lock().unwrap().response = None;
    // A transport that loses the mutation response is covered separately by
    // the shared task_request test. A malformed accepted receipt is equally
    // uncertain and must not cause another submission.
    state
        .lock()
        .unwrap()
        .responses
        .push_back(json!({"operation":{"id":CALL}}));
    state.lock().unwrap().requests.clear();
    assert!(bridge.task_model_submit(&review, true).is_err());
    assert_eq!(
        mutations(&state)
            .iter()
            .filter(|r| r.1.ends_with("/model-runs"))
            .count(),
        1
    );
    state.lock().unwrap().response = Some(
        json!({"task_id":TASK,"operations":[operation()],"retention":"current_profile_session_latest_128","missing_requires_task_inspection":true}),
    );
    let recovered = bridge.task_model_operations(&task).unwrap();
    let handle = bridge
        .task_model_operation_review(&task, &recovered[0])
        .unwrap();
    assert_eq!(handle.operation().id, CALL);
    state.lock().unwrap().response = Some(
        json!({"task_id":TASK,"operations":vec![operation();129],"retention":"current_profile_session_latest_128","missing_requires_task_inspection":true}),
    );
    assert!(bridge.task_model_operations(&task).is_err());
    assert_eq!(
        mutations(&state)
            .iter()
            .filter(|r| r.1.ends_with("/model-runs"))
            .count(),
        1
    );
}

#[test]
fn model_cancel_carries_exact_operation_and_cleanup_failure_is_visible() {
    let (mut bridge, state) = make_bridge();
    let (task, _) = local_review(&mut bridge, &state);
    let handle = bridge
        .task_model_operation_review(&task, &serde_json::from_value(operation()).unwrap())
        .unwrap();
    let mut cancelled = task.snapshot().clone();
    cancelled.task.revision = 1;
    cancelled.task.state = TaskState::Cancelled {
        reason: "explicit".into(),
    };
    state.lock().unwrap().response = Some(serde_json::to_value(cancelled).unwrap());
    state.lock().unwrap().requests.clear();
    bridge
        .task_model_cancel(&task, &handle, "explicit")
        .unwrap();
    assert_eq!(
        mutations(&state)[0].2.as_ref().unwrap(),
        &json!({"expected_revision":0,"reason":"explicit","model_operation_id":CALL})
    );
    let mut failed = operation();
    failed["phase"] = json!("failed");
    failed["worker_drained"] = json!(true);
    failed["cleanup_unconfirmed"] = json!(true);
    failed["stop_reason"] = json!("failed");
    failed["task_revision"] = json!(1);
    failed["error"] =
        json!({"code":"cleanup_unconfirmed","message":"Owned cleanup remains unconfirmed"});
    state.lock().unwrap().response = Some(json!({"operation":failed.clone()}));
    let status = bridge.task_model_poll(&handle).unwrap();
    assert_eq!(status.phase, ModelRunPhase::Failed);
    assert!(status.cleanup_unconfirmed);
    assert!(status.worker_drained);
    assert!(status.cancellation.is_none());
    failed["phase"] = json!("cancelled");
    state.lock().unwrap().response = Some(json!({"operation":failed}));
    assert!(bridge.task_model_poll(&handle).is_err());
}

#[test]
fn checked_submit_distinguishes_preflight_rejection_from_lost_post_response() {
    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    state.lock().unwrap().requests.clear();
    assert!(matches!(
        bridge.task_model_submit_checked(&review, false),
        Err(ModelSubmitFailure::NotSubmitted(_))
    ));
    assert!(mutations(&state).is_empty());
    state.lock().unwrap().response = Some(model_result(model.config(), "c"));
    assert!(matches!(
        bridge.task_model_submit_checked(&review, true),
        Err(ModelSubmitFailure::NotSubmitted(_))
    ));
    assert!(!mutations(&state)
        .iter()
        .any(|r| r.1.ends_with("/model-runs")));

    state.lock().unwrap().responses =
        std::collections::VecDeque::from([model_result(model.config(), "b"), operations(vec![])]);
    state.lock().unwrap().async_failure = true;
    let Err(ModelSubmitFailure::OutcomeUnknown { recovery, .. }) =
        bridge.task_model_submit_checked(&review, true)
    else {
        panic!("a lost POST must remain unknown")
    };
    assert_eq!(recovery.after_sequence(), 0);
    assert_eq!(recovery.review().task().snapshot().task.id, TASK);
    assert_eq!(
        mutations(&state)
            .iter()
            .filter(|r| r.1.ends_with("/model-runs"))
            .count(),
        1
    );
}

#[test]
fn unknown_submit_recovers_only_one_newer_exact_receipt_in_the_same_session() {
    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    // A failure before durable reservation leaves the submitted revision intact.
    let mut old = operation();
    old["sequence"] = json!(5);
    old["phase"] = json!("failed");
    old["stop_reason"] = json!("failed");
    old["error"] = json!({"code":"budget_exceeded","message":"No model step began"});
    old["worker_drained"] = json!(true);
    old["task_revision"] = json!(0);
    state.lock().unwrap().responses = std::collections::VecDeque::from([
        model_result(model.config(), "b"),
        operations(vec![old.clone()]),
    ]);
    state.lock().unwrap().async_failure = true;
    let Err(ModelSubmitFailure::OutcomeUnknown { recovery, .. }) =
        bridge.task_model_submit_checked(&review, true)
    else {
        panic!("a lost POST must retain its watermark")
    };
    assert_eq!(recovery.after_sequence(), 5);
    state.lock().unwrap().response = Some(operations(vec![old.clone()]));
    assert!(bridge.task_model_recover(&task, &recovery).is_err());
    let mut new = operation();
    new["id"] = json!(OTHER);
    new["sequence"] = json!(6);
    state.lock().unwrap().response = Some(operations(vec![old.clone(), new.clone()]));
    assert_eq!(
        bridge
            .task_model_recover(&task, &recovery)
            .unwrap()
            .operation()
            .id,
        OTHER
    );
    let mut another = new.clone();
    another["id"] = json!(WS);
    another["sequence"] = json!(7);
    state.lock().unwrap().response = Some(operations(vec![old.clone(), new.clone(), another]));
    assert!(bridge.task_model_recover(&task, &recovery).is_err());
    new["binding_id"] = json!(format!("model:{}", "c".repeat(64)));
    state.lock().unwrap().response = Some(operations(vec![old, new]));
    assert!(bridge.task_model_recover(&task, &recovery).is_err());
    let mut other_task = task.snapshot().clone();
    other_task.task.id = OTHER.into();
    let other_task = bridge.task_review(&other_task).unwrap();
    assert!(bridge.task_model_recover(&other_task, &recovery).is_err());
    state.lock().unwrap().revoked = true;
    assert!(bridge.task_model_recover(&task, &recovery).is_err());
    assert!(!bridge.view.available);
    assert_eq!(
        mutations(&state)
            .iter()
            .filter(|r| r.1.ends_with("/model-runs"))
            .count(),
        1
    );
}

#[test]
fn session_loss_after_post_is_unknown_and_old_admission_receipts_are_rejected() {
    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    state.lock().unwrap().responses = std::collections::VecDeque::from([
        model_result(model.config(), "b"),
        operations(vec![]),
        json!({"operation":operation()}),
    ]);
    state.lock().unwrap().revoke_on_async = true;
    assert!(matches!(
        bridge.task_model_submit_checked(&review, true),
        Err(ModelSubmitFailure::OutcomeUnknown { .. })
    ));
    assert!(!bridge.view.available);

    let (mut bridge, state) = make_bridge();
    let (task, model) = local_review(&mut bridge, &state);
    let review = bridge.task_model_run_review(&task, &model, 1).unwrap();
    state.lock().unwrap().responses = std::collections::VecDeque::from([
        model_result(model.config(), "b"),
        operations(vec![operation()]),
        json!({"operation":operation()}),
    ]);
    assert!(matches!(
        bridge.task_model_submit_checked(&review, true),
        Err(ModelSubmitFailure::OutcomeUnknown { .. })
    ));
}
