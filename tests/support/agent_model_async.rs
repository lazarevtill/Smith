//! Async task admission over actual routers, keyed synthetic profiles and an
//! owned loopback model fixture. No installed model or external service.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request},
};
use smith_profile_storage as storage;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::Semaphore;
use tower::ServiceExt;

const SECRET: &str = "independent synthetic async task profile";
const SENTINEL: &str = "OWNED_ASYNC_TASK_SENTINEL_27c82b";
struct Fixture {
    dir: tempfile::TempDir,
    app: Option<Arc<App>>,
    roots: Vec<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join(storage::DB_FILE);
        let db = storage::create(&path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let app = crate::app_from_connection(db).unwrap();
        recover(&app).unwrap();
        Self {
            dir,
            app: Some(app),
            roots: vec![],
        }
    }
    fn app(&self) -> Arc<App> {
        self.app.as_ref().unwrap().clone()
    }
    async fn call(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
        request(self.app(), method, path, input).await
    }
    async fn ok(&self, method: Method, path: &str, input: Value) -> Value {
        let (status, result) = self.call(method, path, input).await;
        assert!(status.is_success(), "{path}: {status} {result}");
        result
    }
    async fn task(&mut self, selected: &Value, granted: bool) -> (String, Value) {
        let resources = if granted {
            json!([{"kind":"provider_binding","id":selected["scope_id"]}])
        } else {
            json!([])
        };
        let ws = self.ok(Method::POST,"/api/agent/workspaces",json!({"label":"Owned async workspace","capabilities":["model_inference","list","write"],"resources":resources,"confirmed":true})).await;
        let store = AgentStore::new(&self.app()).unwrap();
        let policy = store
            .policy(ws["workspace"]["id"].as_str().unwrap())
            .unwrap();
        self.roots.push(
            Workspace::reopen_temporary(&policy.identity)
                .unwrap()
                .host_path()
                .to_path_buf(),
        );
        let task = self.ok(Method::POST,"/api/agent/tasks",json!({"workspace_id":policy.id,"policy_revision":policy.revision,"input":SENTINEL})).await;
        (task["task"]["id"].as_str().unwrap().into(), task)
    }
    async fn task_status(&self, id: &str) -> Value {
        self.ok(Method::GET, &format!("/api/agent/tasks/{id}"), Value::Null)
            .await
    }
    async fn terminal(&self, id: &str, operation: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let status = self
                    .ok(
                        Method::GET,
                        &format!("/api/agent/tasks/{id}/model-runs/{operation}"),
                        Value::Null,
                    )
                    .await;
                if status["operation"]["phase"] != "running" {
                    return status["operation"].clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned operation must terminate")
    }
    async fn restart(&mut self) {
        let app = self.app.take().unwrap();
        shutdown(&app).await.unwrap();
        app.tasks.drain().await;
        let app = Arc::try_unwrap(app)
            .ok()
            .expect("all tracked workers drained");
        storage::close(app.db.into_inner().unwrap()).unwrap();
        let db = storage::unlock(
            &self
                .dir
                .path()
                .canonicalize()
                .unwrap()
                .join(storage::DB_FILE),
            &storage::Secret::new(SECRET.into()).unwrap(),
        )
        .unwrap();
        let app = crate::app_from_connection(db).unwrap();
        recover(&app).unwrap();
        self.app = Some(app);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for root in &self.roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
async fn request(app: Arc<App>, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
    let response = routes()
        .with_state(app)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
struct ModelFixture {
    gate: Arc<Semaphore>,
    entered: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<Value>>>,
}
impl Drop for ModelFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl ModelFixture {
    async fn select(f: &Fixture, replies: Vec<Value>) -> (Self, Value, Value) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let held = gate.clone();
        let entered = Arc::new(AtomicUsize::new(0));
        let counted = entered.clone();
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let router = Router::new()
            .route(
                "/v1/models",
                get(|| async { Json(json!({"data":[{"id":"owned-async-model"}]})) }),
            )
            .route(
                "/v1/chat/completions",
                post(move |Json(body): Json<Value>| {
                    let gate = held.clone();
                    let count = counted.clone();
                    let replies = replies.clone();
                    let requests = captured.clone();
                    async move {
                        assert_eq!(body["model"], "owned-async-model");
                        requests.lock().unwrap().push(body);
                        count.fetch_add(1, Ordering::SeqCst);
                        let reply = replies
                            .lock()
                            .unwrap()
                            .pop_front()
                            .expect("no unexpected retry");
                        gate.acquire().await.unwrap().forget();
                        Json(reply)
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let app = f.app();
        let config = crate::Config {
            provider: "openai".into(),
            endpoint: format!("http://{address}/v1"),
            model: "owned-async-model".into(),
        };
        let _ = crate::set_config(State(app.clone()), Json(config.clone()))
            .await
            .unwrap();
        let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":config.provider,"endpoint":config.endpoint});
        let _ = crate::test_connection(
            State(app),
            Json(serde_json::from_value(expected.clone()).unwrap()),
        )
        .await
        .unwrap();
        let selected = f
            .ok(Method::POST, "/api/agent/model-selection", expected.clone())
            .await;
        (
            Self {
                gate,
                entered,
                server,
                requests,
            },
            expected,
            selected,
        )
    }
    async fn wait_entered(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.entered.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
fn reply(call: Value) -> Value {
    json!({"choices":[{"finish_reason":"stop","message":{"content":json!({"content":"Synthetic proposed step","tool_call":call}).to_string()}}]})
}
fn input(task: &Value, expected: &Value, selected: &Value) -> Value {
    json!({"expected_revision":task["task"]["revision"],"expected":expected,"max_steps":1,"confirmed":true,"binding_id":selected["binding_id"]})
}
fn write_reply() -> Value {
    reply(json!({"name":"write_file","arguments":{"path":"only-approved.txt","contents":SENTINEL}}))
}

#[tokio::test]
async fn admission_returns_before_blocked_inference_duplicate_refused_and_exact_approval_survives_restart(
) {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(&f, vec![write_reply()]).await;
    let (id, task) = f.task(&selected, true).await;
    let path = format!("/api/agent/tasks/{id}/model-runs");
    let body = input(&task, &expected, &selected);
    let (status, admitted) = tokio::time::timeout(
        Duration::from_millis(250),
        f.call(Method::POST, &path, body.clone()),
    )
    .await
    .expect("admission must not await inference");
    assert_eq!(status, StatusCode::ACCEPTED);
    let op = admitted["operation"]["id"].as_str().unwrap();
    model.wait_entered(1).await;
    assert_eq!(
        f.ok(Method::GET, &format!("{path}/{op}"), Value::Null)
            .await["operation"]["phase"],
        "running"
    );
    assert_eq!(
        f.call(Method::POST, &path, body.clone()).await.0,
        StatusCode::CONFLICT
    );
    let mut sync_body = body;
    sync_body.as_object_mut().unwrap().remove("binding_id");
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-run"),
            sync_body
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    model.gate.add_permits(1);
    let terminal = f.terminal(&id, op).await;
    assert_eq!(terminal["phase"], "succeeded");
    assert_eq!(terminal["stop_reason"], "needs_approval");
    assert!(!terminal.to_string().contains(SENTINEL));
    assert!(f.app().agent.work.inner.lock().unwrap().jobs.is_empty());
    let pending = f.task_status(&id).await;
    assert_eq!(pending["task"]["state"]["kind"], "needs_approval");
    assert!(!f.roots[0].join("only-approved.txt").exists());
    let other = Fixture::new();
    assert_eq!(
        other
            .call(Method::GET, &format!("{path}/{op}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    f.restart().await;
    assert_eq!(
        f.call(Method::GET, &format!("{path}/{op}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(f.ok(Method::GET, &path, Value::Null).await["operations"]
        .as_array()
        .unwrap()
        .is_empty());
    let restored = f.task_status(&id).await;
    assert_eq!(restored["task"]["state"], pending["task"]["state"]);
    f.ok(Method::POST,&format!("/api/agent/tasks/{id}/approval"),json!({"expected_revision":restored["task"]["revision"],"call_id":restored["task"]["state"]["call"]["id"],"decision":"approve"})).await;
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("only-approved.txt")).unwrap(),
        SENTINEL
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_drains_model_before_success_and_late_reply_never_dispatches() {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(&f, vec![write_reply()]).await;
    let (id, task) = f.task(&selected, true).await;
    let admitted = f
        .ok(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-runs"),
            input(&task, &expected, &selected),
        )
        .await;
    model.wait_entered(1).await;
    let thinking = f.task_status(&id).await;
    let cancelled=tokio::time::timeout(Duration::from_secs(3),f.ok(Method::POST,&format!("/api/agent/tasks/{id}/cancel"),json!({"expected_revision":thinking["task"]["revision"],"reason":"Explicit owned cancellation"}))).await.unwrap();
    assert_eq!(cancelled["task"]["state"]["kind"], "cancelled");
    let terminal = f
        .terminal(&id, admitted["operation"]["id"].as_str().unwrap())
        .await;
    assert_eq!(terminal["phase"], "cancelled");
    assert!(f.app().agent.work.inner.lock().unwrap().jobs.is_empty());
    model.gate.add_permits(1);
    f.app().tasks.drain().await;
    let after = f.task_status(&id).await;
    assert_eq!(after["task"]["revision"], cancelled["task"]["revision"]);
    assert!(after["task"]["last_model_reply"].is_null());
    assert!(!f.roots[0].join("only-approved.txt").exists());
}

#[tokio::test]
async fn dropped_response_retains_worker_and_shutdown_cancels_and_joins() {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(&f, vec![write_reply()]).await;
    let (id, task) = f.task(&selected, true).await;
    let app = f.app();
    let response = routes()
        .with_state(app.clone())
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/agent/tasks/{id}/model-runs"))
                .header("content-type", "application/json")
                .body(Body::from(input(&task, &expected, &selected).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    drop(response);
    model.wait_entered(1).await;
    let operations = f
        .ok(
            Method::GET,
            &format!("/api/agent/tasks/{id}/model-runs"),
            Value::Null,
        )
        .await;
    let op = operations["operations"][0]["id"].as_str().unwrap();
    tokio::time::timeout(Duration::from_secs(3), shutdown(&app))
        .await
        .unwrap()
        .unwrap();
    app.tasks.drain().await;
    assert_eq!(f.terminal(&id, op).await["phase"], "cancelled");
    assert!(app.agent.work.inner.lock().unwrap().jobs.is_empty());
    assert!(!f.roots[0].join("only-approved.txt").exists());
    assert!(app.agent.begin_model(&id, None).is_err());
}

#[tokio::test]
async fn invalid_stale_ungranted_and_changed_binding_requests_dispatch_nothing() {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(&f, vec![]).await;
    let (id, task) = f.task(&selected, true).await;
    let path = format!("/api/agent/tasks/{id}/model-runs");
    let original = input(&task, &expected, &selected);
    for (key, value) in [
        ("max_steps", json!(0)),
        ("max_steps", json!(9)),
        ("confirmed", json!(false)),
        ("expected_revision", json!(99)),
        ("binding_id", json!("model:changed")),
        ("unexpected", json!(true)),
    ] {
        let mut changed = original.clone();
        changed[key] = value;
        assert!(!f.call(Method::POST, &path, changed).await.0.is_success());
    }
    let (id2, task2) = f.task(&selected, false).await;
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id2}/model-runs"),
            input(&task2, &expected, &selected)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 0);
    assert!(f
        .app()
        .agent
        .work
        .inner
        .lock()
        .unwrap()
        .model_operations
        .is_empty());
}

#[tokio::test]
async fn partial_reply_and_unavailable_tool_are_terminal_failures_without_dispatch() {
    let mut f = Fixture::new();
    let mut partial = write_reply();
    partial["choices"][0]["finish_reason"] = json!("length");
    let (model, expected, selected) = ModelFixture::select(
        &f,
        vec![
            partial,
            reply(json!({"name":"run_shell","arguments":{"command":"never"}})),
        ],
    )
    .await;
    for n in 1..=2 {
        let (id, task) = f.task(&selected, true).await;
        let admitted = f
            .ok(
                Method::POST,
                &format!("/api/agent/tasks/{id}/model-runs"),
                input(&task, &expected, &selected),
            )
            .await;
        model.wait_entered(n).await;
        model.gate.add_permits(1);
        let terminal = f
            .terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await;
        assert_eq!(terminal["phase"], "failed");
        assert_eq!(terminal["error"]["code"], "invalid_input");
        assert!(terminal["error"]["message"].as_str().unwrap().len() <= 2048);
        let task = f.task_status(&id).await;
        assert!(task["task"]["last_model_reply"].is_null());
        assert_eq!(
            task["task"]["last_model_rejection"]["reply"]["reason"],
            if n == 1 {
                "incomplete"
            } else {
                "unavailable_tool_or_arguments"
            }
        );
        assert!(!f.roots[n - 1].join("only-approved.txt").exists());
    }
}

#[tokio::test]
async fn rejected_text_is_encrypted_durable_inert_and_absent_from_the_next_model_context() {
    const REJECTED: &str = "REJECTED_OUTPUT_PRIVATE_SENTINEL_e97a";
    let mut f = Fixture::new();
    let raw = format!("```json\n{{\"content\":\"{REJECTED}\",\"tool_call\":{{\"name\":\"write_file\",\"arguments\":{{\"path\":\"never.txt\",\"contents\":\"never\"}}}}}}\n```");
    let (model, expected, selected) = ModelFixture::select(
        &f,
        vec![
            json!({"choices":[{"finish_reason":"stop","message":{"content":raw}}]}),
            reply(Value::Null),
        ],
    )
    .await;
    let (id, task) = f.task(&selected, true).await;
    let path = format!("/api/agent/tasks/{id}/model-runs");
    let admitted = f
        .ok(Method::POST, &path, input(&task, &expected, &selected))
        .await;
    model.wait_entered(1).await;
    model.gate.add_permits(1);
    let terminal = f
        .terminal(&id, admitted["operation"]["id"].as_str().unwrap())
        .await;
    assert_eq!(terminal["phase"], "failed");
    assert!(!terminal.to_string().contains(REJECTED));
    let rejected = f.task_status(&id).await;
    let diagnostic = rejected["task"]["last_model_rejection"].clone();
    assert_eq!(diagnostic["reply"]["reason"], "invalid_structured_step");
    assert_eq!(diagnostic["reply"]["text"], raw);
    assert_eq!(diagnostic["reply"]["original_bytes"], raw.len());
    assert_eq!(diagnostic["reply"]["truncated"], false);
    assert!(rejected["task"]["last_model_reply"].is_null());
    assert!(!f.roots[0].join("never.txt").exists());
    let trace = f
        .ok(
            Method::GET,
            &format!("/api/agent/tasks/{id}/trace"),
            Value::Null,
        )
        .await;
    let events = trace["events"].as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["event"]["kind"] == "model_rejected")
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|e| e["event"]["kind"] == "tool_started" || e["event"]["kind"] == "model_finished"));
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    f.restart().await;
    let reopened = f.task_status(&id).await;
    assert_eq!(reopened["task"]["last_model_rejection"], diagnostic);
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    for file in std::fs::read_dir(f.dir.path()).unwrap() {
        let path = file.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(path).unwrap();
            for sentinel in [REJECTED, SECRET] {
                assert!(!bytes
                    .windows(sentinel.len())
                    .any(|v| v == sentinel.as_bytes()));
            }
        }
    }
    // A durably rejected step is idle. Reopening does not manufacture an
    // interruption; the next explicit run still binds its current revision.
    assert_eq!(reopened["task"]["state"]["kind"], "running");
    let app = f.app();
    let config = app.config.lock().unwrap().clone();
    let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":config.provider,"endpoint":config.endpoint});
    let _ = crate::test_connection(
        State(app),
        Json(serde_json::from_value(expected.clone()).unwrap()),
    )
    .await
    .unwrap();
    let selected = f
        .ok(Method::POST, "/api/agent/model-selection", expected.clone())
        .await;
    let admitted = f
        .ok(Method::POST, &path, input(&reopened, &expected, &selected))
        .await;
    model.wait_entered(2).await;
    model.gate.add_permits(1);
    assert_eq!(
        f.terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await["phase"],
        "succeeded"
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].to_string().contains(REJECTED));
    assert!(!requests[1].to_string().contains("model_rejected"));
}

#[tokio::test]
async fn oversized_rejected_reply_has_a_bounded_utf8_prefix_and_full_observed_hash() {
    let mut f = Fixture::new();
    let raw = "\0🐟\"".repeat(6000);
    let (model, expected, selected) = ModelFixture::select(
        &f,
        vec![json!({"choices":[{"finish_reason":"stop","message":{"content":raw}}]})],
    )
    .await;
    let (id, task) = f.task(&selected, true).await;
    let admitted = f
        .ok(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-runs"),
            input(&task, &expected, &selected),
        )
        .await;
    model.wait_entered(1).await;
    model.gate.add_permits(1);
    assert_eq!(
        f.terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await["phase"],
        "failed"
    );
    let task = f.task_status(&id).await;
    let evidence: smith_agent_core::RejectedModelReply =
        serde_json::from_value(task["task"]["last_model_rejection"]["reply"].clone()).unwrap();
    evidence.validate().unwrap();
    assert_eq!(
        evidence.reason,
        smith_agent_core::ModelRejectionReason::ResponseLimit
    );
    assert_eq!(evidence.original_bytes, raw.len() as u64);
    assert_eq!(
        evidence.sha256,
        smith_agent_core::RejectedModelReply::capture(evidence.reason, &raw, Value::Null)
            .unwrap()
            .sha256
    );
    assert!(evidence.truncated && raw.starts_with(&evidence.text));
    assert!(serde_json::to_vec(&evidence).unwrap().len() <= 20 * 1024);
    assert!(task["task"]["last_model_reply"].is_null());
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_reply_storage_failure_stays_uncertain_and_never_claims_recorded_evidence() {
    let mut f = Fixture::new();
    let (model,expected,selected)=ModelFixture::select(&f,vec![json!({"choices":[{"finish_reason":"stop","message":{"content":"synthetic invalid step"}}]})]).await;
    let (id, task) = f.task(&selected, true).await;
    f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER reject_diagnostic BEFORE INSERT ON agent_task_trace WHEN NEW.event LIKE '%model_rejected%' BEGIN SELECT RAISE(ABORT,'synthetic diagnostic persistence refusal'); END;").unwrap();
    let admitted = f
        .ok(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-runs"),
            input(&task, &expected, &selected),
        )
        .await;
    model.wait_entered(1).await;
    model.gate.add_permits(1);
    let terminal = f
        .terminal(&id, admitted["operation"]["id"].as_str().unwrap())
        .await;
    assert_eq!(terminal["phase"], "failed");
    assert_eq!(terminal["error"]["code"], "storage");
    let pending = f.task_status(&id).await;
    assert_eq!(pending["task"]["state"]["kind"], "model_thinking");
    assert!(pending["task"]["last_model_rejection"].is_null());
    assert!(pending["task"]["last_model_reply"].is_null());
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    f.app()
        .db
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_diagnostic")
        .unwrap();
    f.restart().await;
    assert_eq!(
        f.task_status(&id).await["task"]["state"]["kind"],
        "interrupted"
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn iteration_limit_requires_new_explicit_revision_and_does_not_repeat() {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(
        &f,
        vec![
            reply(json!({"name":"list_directory","arguments":{}})),
            reply(Value::Null),
        ],
    )
    .await;
    let (id, task) = f.task(&selected, true).await;
    let path = format!("/api/agent/tasks/{id}/model-runs");
    let original = input(&task, &expected, &selected);
    let admitted = f.ok(Method::POST, &path, original.clone()).await;
    model.wait_entered(1).await;
    model.gate.add_permits(1);
    assert_eq!(
        f.terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await["stop_reason"],
        "iteration_limit"
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.call(Method::POST, &path, original).await.0,
        StatusCode::CONFLICT
    );
    let current = f.task_status(&id).await;
    let admitted = f
        .ok(Method::POST, &path, input(&current, &expected, &selected))
        .await;
    model.wait_entered(2).await;
    model.gate.add_permits(1);
    assert_eq!(
        f.terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await["stop_reason"],
        "completed"
    );
    assert_eq!(
        f.ok(Method::GET, &path, Value::Null).await["operations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn cancellation_between_admission_and_first_worker_poll_sends_no_model_request() {
    let mut f = Fixture::new();
    let (model, expected, selected) = ModelFixture::select(&f, vec![]).await;
    let (id, task) = f.task(&selected, true).await;
    let app = f.app();
    // This current-thread executor cannot poll the spawned worker until this
    // task yields. Admission has no await; cancel commits and signals first.
    let (_, Json(admitted)) = model_run_async(
        State(app.clone()),
        Path(id.clone()),
        Ok(Json(
            serde_json::from_value(input(&task, &expected, &selected)).unwrap(),
        )),
    )
    .await
    .unwrap();
    let Json(cancelled) = cancel(
        State(app.clone()),
        Path(id.clone()),
        Ok(Json(Cancel {
            expected_revision: 0,
            reason: "Cancel before dispatch".into(),
            model_operation_id: None,
        })),
    )
    .await
    .unwrap();
    assert_eq!(cancelled["task"]["state"]["kind"], "cancelled");
    assert_eq!(
        f.terminal(&id, admitted["operation"]["id"].as_str().unwrap())
            .await["phase"],
        "cancelled"
    );
    assert_eq!(model.entered.load(Ordering::SeqCst), 0);
    assert_eq!(cancelled["task"]["model_steps"], 0);
}

fn metadata(task: &str) -> ModelOperation {
    ModelOperation {
        id: String::new(),
        task_id: task.into(),
        submitted_revision: 0,
        max_steps: 1,
        scope_id: format!("provider:{}", "a".repeat(64)),
        binding_id: format!("model:{}", "b".repeat(64)),
        sequence: 0,
        state: ModelTerminal {
            phase: ModelPhase::Running,
            task_revision: None,
            stop_reason: "running",
            error: None,
            worker_drained: false,
            cleanup_unconfirmed: false,
            cancellation: None,
        },
    }
}
#[test]
fn bounded_ephemeral_status_never_evicts_active_and_abandoned_lease_is_failed() {
    let state = AgentState::default();
    let (active, _) = state
        .begin_model("active", Some(metadata("active")))
        .unwrap();
    let active_id = active.id.clone();
    assert!(matches!(
        state.begin_model("active", None),
        Err(AgentError(Error::Conflict))
    ));
    let mut first = String::new();
    for n in 0..MAX_MODEL_OPERATIONS + 4 {
        let task = format!("terminal-{n}");
        let (lease, _) = state.begin_model(&task, Some(metadata(&task))).unwrap();
        if n == 0 {
            first = lease.id.clone();
        }
        lease.finish_model(ModelTerminal {
            phase: ModelPhase::Succeeded,
            task_revision: Some(2),
            stop_reason: "completed",
            error: None,
            worker_drained: true,
            cleanup_unconfirmed: false,
            cancellation: None,
        });
    }
    {
        let inner = state.work.inner.lock().unwrap();
        assert_eq!(inner.model_operations.len(), MAX_MODEL_OPERATIONS);
        assert!(inner.model_operations.contains_key(&active_id));
        assert!(!inner.model_operations.contains_key(&first));
        assert_eq!(inner.jobs.len(), 1);
    }
    drop(active);
    let inner = state.work.inner.lock().unwrap();
    assert!(inner.jobs.is_empty());
    assert_eq!(
        inner.model_operations[&active_id].state.phase as u8,
        ModelPhase::Failed as u8
    );
    assert_eq!(
        inner.model_operations[&active_id]
            .state
            .error
            .as_ref()
            .unwrap()
            .code,
        "worker_stopped"
    );
}

#[tokio::test]
async fn cancelled_task_keeps_cleanup_or_storage_failure_and_cancel_never_claims_success() {
    let mut f = Fixture::new();
    let (id, _) = f
        .task(
            &json!({"scope_id":format!("provider:{}","a".repeat(64))}),
            true,
        )
        .await;
    let app = f.app();
    let (lease, mut cancelled) = app.agent.begin_model(&id, Some(metadata(&id))).unwrap();
    let operation = lease.id.clone();
    let worker_app = app.clone();
    let worker_id = id.clone();
    app.spawn_task(async move {
        cancelled.changed().await.unwrap();
        let failure = GuidedFailure::from(crate::task_model::CompletionFailure::Cleanup(
            crate::native_api::failure(smith_managed_runtime::Failure::Cleanup),
        ));
        lease.finish_model(model_terminal(&worker_app, &worker_id, &Err(failure)));
    });
    let response = f
        .call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/cancel"),
            json!({"expected_revision":0,"reason":"Synthetic cleanup failure"}),
        )
        .await;
    assert_eq!(response.0, StatusCode::CONFLICT);
    let terminal = f.terminal(&id, &operation).await;
    assert_eq!(terminal["phase"], "failed");
    assert_eq!(terminal["cleanup_unconfirmed"], true);
    assert_eq!(terminal["worker_drained"], true);
    assert_eq!(terminal["error"]["code"], "cleanup_unconfirmed");
    assert!(terminal["cancellation"].is_null());
    assert_eq!(
        f.task_status(&id).await["task"]["state"]["kind"],
        "cancelled"
    );
    let storage = model_terminal(
        &app,
        &id,
        &Err(Error::Persistence("synthetic failed durable commit".into()).into()),
    );
    assert!(storage.phase == ModelPhase::Failed);
    assert_eq!(storage.error.unwrap().code, "storage");
    app.tasks.drain().await;
}

#[tokio::test]
async fn wrong_operation_cannot_cancel_later_admission_or_mutate_task() {
    let mut f = Fixture::new();
    let (id, task) = f
        .task(
            &json!({"scope_id":format!("provider:{}","a".repeat(64))}),
            true,
        )
        .await;
    let app = f.app();
    let (old, _) = app.agent.begin_model(&id, Some(metadata(&id))).unwrap();
    let stale = old.id.clone();
    old.finish_model(ModelTerminal {
        phase: ModelPhase::Succeeded,
        task_revision: Some(0),
        stop_reason: "iteration_limit",
        error: None,
        worker_drained: true,
        cleanup_unconfirmed: false,
        cancellation: None,
    });
    let (current, cancelled) = app.agent.begin_model(&id, Some(metadata(&id))).unwrap();
    let input = json!({"expected_revision":0,"reason":"Stale operation must not cancel","model_operation_id":stale});
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/cancel"),
            input
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(f.task_status(&id).await["task"], task["task"]);
    assert!(!*cancelled.borrow());
    drop(current);
}

#[tokio::test]
async fn completion_during_cancel_commit_cannot_escape_its_drain_watch() {
    let mut f = Fixture::new();
    let (id, _) = f
        .task(
            &json!({"scope_id":format!("provider:{}","a".repeat(64))}),
            true,
        )
        .await;
    let app = f.app();
    let (lease, _) = app.agent.begin_model(&id, Some(metadata(&id))).unwrap();
    let operation = lease.id.clone();
    let store = AgentStore::new(&app).unwrap();
    let engine = TaskEngine::new(store.clone(), store.registry().unwrap());
    let worker_app = app.clone();
    let worker_id = id.clone();
    let mut worker = None;
    let waiters = app
        .agent
        .cancel_model(&id, Some(&operation), || {
            engine.cancel_owned_at(
                &id,
                0,
                store.owner(),
                "Synthetic simultaneous cleanup failure",
            )?;
            let (entered, received) = std::sync::mpsc::channel();
            worker = Some(std::thread::spawn(move || {
                let terminal = model_terminal(
                    &worker_app,
                    &worker_id,
                    &Err(GuidedFailure::from(
                        crate::task_model::CompletionFailure::Cleanup(crate::mlx_api::failure(
                            smith_mlx_runtime::Failure::Cleanup,
                        )),
                    )),
                );
                entered.send(()).unwrap();
                lease.finish_model(terminal);
            }));
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(waiters.len(), 1);
    worker.unwrap().join().unwrap();
    let drained = app.agent.drain_model(waiters).await.unwrap();
    assert!(drained[0].cleanup_unconfirmed);
    assert!(drained[0].worker_drained);
    assert!(drained[0].phase == ModelPhase::Failed);
    assert_eq!(
        drained[0].error.as_ref().unwrap().code,
        "cleanup_unconfirmed"
    );
    assert_eq!(
        f.task_status(&id).await["task"]["state"]["kind"],
        "cancelled"
    );
}
