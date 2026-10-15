//! Synthetic endpoints on real SQLCipher; local HTTP returns scripted text only.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request, StatusCode},
};
use smith_agent_core::Workspace;
use smith_profile_storage as storage;
use std::{path::PathBuf, sync::Mutex};
use tower::ServiceExt;

const SECRET: &str = "synthetic saved-agent encryption passphrase";
const PERSONA: &str = "PRIVATE_SAVED_AGENT_PERSONA_408e write briefly, never invent evidence";
struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    app: Option<Arc<App>>,
    roots: Vec<PathBuf>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join(storage::DB_FILE);
        let db = storage::create(&path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let app = crate::app_from_connection(db).unwrap();
        agent_api::recover(&app).unwrap();
        Self {
            dir,
            path,
            app: Some(app),
            roots: vec![],
        }
    }
    fn app(&self) -> Arc<App> {
        self.app.as_ref().unwrap().clone()
    }
    async fn request(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(if input.is_null() {
                Body::empty()
            } else {
                Body::from(input.to_string())
            })
            .unwrap();
        let response = routes()
            .merge(agent_api::routes())
            .with_state(self.app())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn ok(&self, method: Method, path: &str, input: Value) -> Value {
        let (status, value) = self.request(method, path, input).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }
    async fn config(&mut self, caps: Value, resources: Value) -> Value {
        let workspace=self.ok(Method::POST,"/api/agent/workspaces",json!({"label":"Synthetic saved-agent workspace","capabilities":caps,"resources":resources,"confirmed":true})).await;
        let id = workspace["workspace"]["id"].as_str().unwrap();
        let policy = AgentStore::new(&self.app()).unwrap().policy(id).unwrap();
        self.roots.push(
            Workspace::reopen_temporary(&policy.identity)
                .unwrap()
                .host_path()
                .to_owned(),
        );
        json!({"name":"Synthetic reviewer","system_prompt":PERSONA,"soul":"Use evidence from the selected workspace.","workspace_id":id,"policy_revision":1,"capabilities":caps,"resources":resources,"model":{"mode":"selected_at_launch"}})
    }
    async fn create(&self, config: Value) -> Value {
        self.ok(
            Method::POST,
            "/api/agent/definitions",
            json!({"definition":config,"confirmed":true}),
        )
        .await["definition"]
            .clone()
    }
    async fn launch(&self, definition: &Value, input: Value) -> Value {
        self.ok(
            Method::POST,
            &format!(
                "/api/agent/definitions/{}/launch",
                definition["id"].as_str().unwrap()
            ),
            input,
        )
        .await
    }
    fn counts(&self) -> (i64, i64, i64) {
        let app = self.app();
        let db = app.db.lock().unwrap();
        db.query_row("SELECT (SELECT count(*) FROM agent_tasks),(SELECT count(*) FROM agent_task_trace),(SELECT count(*) FROM agent_definition_launches)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap()
    }
    async fn restart(&mut self) {
        let app = self.app.take().unwrap();
        agent_api::shutdown(&app).await.unwrap();
        app.tasks.drain().await;
        let app = Arc::try_unwrap(app)
            .ok()
            .expect("owned app requests drained");
        storage::close(app.db.into_inner().unwrap()).unwrap();
        let db =
            storage::unlock(&self.path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let app = crate::app_from_connection(db).unwrap();
        agent_api::recover(&app).unwrap();
        self.app = Some(app);
    }
    fn encrypted(&self) {
        for file in std::fs::read_dir(self.dir.path()).unwrap() {
            let path = file.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(path).unwrap();
            assert!(!bytes.starts_with(b"SQLite format 3"));
            for text in [PERSONA, SECRET, "Synthetic reviewer"] {
                assert!(!bytes.windows(text.len()).any(|b| b == text.as_bytes()));
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.app.take();
        for root in &self.roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
fn launch_body(revision: u64) -> Value {
    json!({"request_id":crate::id(),"expected_revision":revision,"input":"Inspect selected files only","confirmed":true})
}

#[tokio::test]
async fn definitions_crud_cas_restart_and_no_automatic_tasks() {
    let mut f = Fixture::new();
    let config = f.config(json!(["list"]), json!([])).await;
    let created = f.create(config.clone()).await;
    let path = format!("/api/agent/definitions/{}", created["id"].as_str().unwrap());
    assert_eq!(created["revision"], 1);
    assert_eq!(f.counts(), (0, 0, 0));
    let unchanged = f
        .ok(
            Method::PUT,
            &path,
            json!({"expected_revision":1,"definition":config,"confirmed":true}),
        )
        .await;
    assert_eq!(unchanged["definition"]["revision"], 1);
    let mut edited = config.clone();
    edited["name"] = json!("Edited reviewer");
    let update = json!({"expected_revision":1,"definition":edited,"confirmed":true});
    let (a, b) = tokio::join!(
        f.request(Method::PUT, &path, update.clone()),
        f.request(Method::PUT, &path, update)
    );
    assert!([a.0, b.0].contains(&StatusCode::OK));
    assert!([a.0, b.0].contains(&StatusCode::CONFLICT));
    assert_eq!(
        f.request(Method::DELETE, &path, json!({"expected_revision":1}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.restart().await;
    let saved = f.ok(Method::GET, &path, Value::Null).await;
    assert_eq!(saved["definition"]["revision"], 2);
    assert_eq!(saved["definition"]["definition"]["system_prompt"], PERSONA);
    assert_eq!(saved["definition"]["created_at"], created["created_at"]);
    assert_eq!(f.counts(), (0, 0, 0));
    f.encrypted();
    let other = Fixture::new();
    assert_eq!(
        other.request(Method::GET, &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    f.ok(Method::DELETE, &path, json!({"expected_revision":2}))
        .await;
    assert_eq!(
        f.ok(Method::GET, "/api/agent/definitions", Value::Null)
            .await["definitions"],
        json!([])
    );
    assert_eq!(
        f.request(Method::GET, &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn definitions_reject_unknown_or_wider_grants_and_malformed_persisted_data() {
    let mut f = Fixture::new();
    let config = f.config(json!(["list"]), json!([])).await;
    for (field, value, expected) in [
        ("capabilities", json!(["write"]), StatusCode::FORBIDDEN),
        ("capabilities", json!(["admin"]), StatusCode::BAD_REQUEST),
        (
            "resources",
            json!([{"kind":"provider_binding","id":"ungranted"}]),
            StatusCode::FORBIDDEN,
        ),
        (
            "system_prompt",
            json!("x".repeat(8193)),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        ("policy_revision", json!(2), StatusCode::CONFLICT),
    ] {
        let mut bad = config.clone();
        bad[field] = value;
        assert_eq!(
            f.request(
                Method::POST,
                "/api/agent/definitions",
                json!({"definition":bad,"confirmed":true})
            )
            .await
            .0,
            expected
        );
    }
    let mut spoof = config.clone();
    spoof["principal"] = json!("admin");
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/definitions",
            json!({"definition":spoof,"confirmed":true})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let saved = f.create(config.clone()).await;
    let id = saved["id"].as_str().unwrap();
    let mut corrupt = config;
    corrupt["capabilities"] = json!(["admin"]);
    {
        let app = f.app();
        let db = app.db.lock().unwrap();
        db.execute(
            "UPDATE agent_definitions SET body_json=?1 WHERE id=?2",
            params![corrupt.to_string(), id],
        )
        .unwrap();
    }
    assert_eq!(
        f.request(
            Method::GET,
            &format!("/api/agent/definitions/{id}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/api/agent/definitions/{id}/launch"),
            launch_body(1)
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(f.counts(), (0, 0, 0));
}

#[tokio::test]
async fn definition_launch_is_exact_once_and_deletion_preserves_narrowed_task_history() {
    let mut f = Fixture::new();
    let mut config = f
        .config(json!(["list", "write", "control"]), json!([]))
        .await;
    config["capabilities"] = json!(["list"]);
    let saved = f.create(config.clone()).await;
    let id = saved["id"].as_str().unwrap();
    let path = format!("/api/agent/definitions/{id}/launch");
    let input = launch_body(1);
    let (a, b) = tokio::join!(
        f.ok(Method::POST, &path, input.clone()),
        f.ok(Method::POST, &path, input.clone())
    );
    assert_eq!(a["task"]["id"], b["task"]["id"]);
    assert_ne!(a["replayed"], b["replayed"]);
    assert_eq!(f.counts(), (1, 1, 1));
    assert_eq!(a["task"]["state"]["kind"], "pending");
    let task_id = a["task"]["id"].as_str().unwrap();
    let task_path = format!("/api/agent/tasks/{task_id}");
    let started = f
        .ok(
            Method::POST,
            &format!("{task_path}/start"),
            json!({"expected_revision":0}),
        )
        .await;
    let listed=f.ok(Method::POST,&format!("{task_path}/actions"),json!({"expected_revision":started["task"]["revision"],"request_id":crate::id(),"action":{"kind":"list"}})).await;
    let denied=f.request(Method::POST,&format!("{task_path}/actions"),json!({"expected_revision":listed["task"]["revision"],"request_id":crate::id(),"action":{"kind":"write","path":"forbidden","contents":"forbidden"}})).await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    assert!(!f.roots[0].join("forbidden").exists());
    config["system_prompt"] = json!("New instructions must not affect an old task");
    config["capabilities"] = json!(["write", "list"]);
    f.ok(
        Method::PUT,
        &format!("/api/agent/definitions/{id}"),
        json!({"expected_revision":1,"definition":config,"confirmed":true}),
    )
    .await;
    assert_eq!(
        f.request(Method::POST, &path, launch_body(1)).await.0,
        StatusCode::CONFLICT
    );
    let mut altered = input.clone();
    altered["input"] = json!("Changed payload");
    assert_eq!(
        f.request(Method::POST, &path, altered).await.0,
        StatusCode::CONFLICT
    );
    f.ok(
        Method::DELETE,
        &format!("/api/agent/definitions/{id}"),
        json!({"expected_revision":2}),
    )
    .await;
    f.restart().await;
    let preserved = f.ok(Method::GET, &task_path, Value::Null).await;
    assert_eq!(preserved["task"]["agent_persona"]["system_prompt"], PERSONA);
    assert_eq!(preserved["task"]["agent_persona"]["definition_revision"], 1);
    assert_eq!(preserved["task"]["capabilities"], json!(["list"]));
    let replay = f.ok(Method::POST, &path, input.clone()).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["task"]["id"], task_id);
    assert_eq!(
        f.request(Method::POST, &path, launch_body(3)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.counts().0, 1);
    assert_eq!(f.counts().2, 1);
    f.ok(
        Method::POST,
        &format!("/api/agent/definitions/{id}/purge"),
        json!({"expected_revision":3,"confirm_id":id,"permanent":true}),
    )
    .await;
    let after_purge = f.ok(Method::POST, &path, input).await;
    assert_eq!(after_purge["task"]["id"], task_id);
    assert_eq!(after_purge["replayed"], true);
    assert_eq!(f.counts().0, 1);
    assert_eq!(f.counts().2, 1);
    f.encrypted();
}

#[tokio::test]
async fn deleted_definitions_restore_by_revision_and_only_explicit_purge_removes_metadata() {
    let mut f = Fixture::new();
    let config = f.config(json!(["list"]), json!([])).await;
    let saved = f.create(config).await;
    let id = saved["id"].as_str().unwrap();
    let path = format!("/api/agent/definitions/{id}");
    assert_eq!(
        f.request(
            Method::POST,
            &format!("{path}/purge"),
            json!({"expected_revision":1,"confirm_id":id,"permanent":true})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    f.ok(Method::DELETE, &path, json!({"expected_revision":1}))
        .await;
    f.restart().await;
    let deleted = f
        .ok(
            Method::GET,
            &format!("{path}?include_deleted=true"),
            Value::Null,
        )
        .await;
    assert_eq!(deleted["definition"]["deleted"], true);
    assert_eq!(deleted["definition"]["revision"], 2);
    assert_eq!(
        f.request(
            Method::POST,
            &format!("{path}/restore"),
            json!({"expected_revision":1})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let restored = f
        .ok(
            Method::POST,
            &format!("{path}/restore"),
            json!({"expected_revision":2}),
        )
        .await;
    assert_eq!(restored["definition"]["deleted"], false);
    assert_eq!(restored["definition"]["revision"], 3);
    assert_eq!(
        restored["definition"]["definition"]["system_prompt"],
        PERSONA
    );
    assert_eq!(f.counts(), (0, 0, 0));
    f.ok(Method::DELETE, &path, json!({"expected_revision":3}))
        .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("{path}/purge"),
            json!({"expected_revision":4,"confirm_id":id,"permanent":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.request(
            Method::POST,
            &format!("{path}/purge"),
            json!({"expected_revision":4,"confirm_id":crate::id(),"permanent":true})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    f.ok(
        Method::POST,
        &format!("{path}/purge"),
        json!({"expected_revision":4,"confirm_id":id,"permanent":true}),
    )
    .await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("{path}/restore"),
            json!({"expected_revision":4})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.ok(
            Method::GET,
            "/api/agent/definitions?include_deleted=true",
            Value::Null
        )
        .await["definitions"],
        json!([])
    );
}

#[tokio::test]
async fn definition_pages_are_bounded_and_explicit_purge_reclaims_capacity() {
    let mut f = Fixture::new();
    let config = f.config(json!(["list"]), json!([])).await;
    let saved = f.create(config.clone()).await;
    {
        let app = f.app();
        let mut db = app.db.lock().unwrap();
        let tx = db.transaction().unwrap();
        for _ in 1..MAX_DEFINITIONS {
            tx.execute("INSERT INTO agent_definitions(id,principal_id,revision,body_json,created_at,updated_at) SELECT ?1,principal_id,1,body_json,created_at,updated_at FROM agent_definitions WHERE id=?2",params![crate::id(),saved["id"].as_str().unwrap()]).unwrap();
        }
        tx.commit().unwrap();
    }
    let mut seen = BTreeSet::new();
    let mut path = "/api/agent/definitions".to_string();
    loop {
        let page = f.ok(Method::GET, &path, Value::Null).await;
        assert!(page.to_string().len() < 2 * 1024 * 1024);
        let rows = page["definitions"].as_array().unwrap();
        assert!(rows.len() <= 32);
        for row in rows {
            assert!(seen.insert(row["id"].as_str().unwrap().to_owned()));
        }
        let Some(after) = page["next_after"].as_str() else {
            break;
        };
        path = format!("/api/agent/definitions?after={after}");
    }
    assert_eq!(seen.len(), MAX_DEFINITIONS as usize);
    assert_eq!(
        f.request(Method::GET, "/api/agent/definitions?limit=33", Value::Null)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/definitions",
            json!({"definition":config,"confirmed":true})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let id = saved["id"].as_str().unwrap();
    f.ok(
        Method::DELETE,
        &format!("/api/agent/definitions/{id}"),
        json!({"expected_revision":1}),
    )
    .await;
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/definitions",
            json!({"definition":config,"confirmed":true})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    f.ok(
        Method::POST,
        &format!("/api/agent/definitions/{id}/purge"),
        json!({"expected_revision":2,"confirm_id":id,"permanent":true}),
    )
    .await;
    f.create(config).await;
    assert_eq!(f.counts(), (0, 0, 0));
}

#[tokio::test]
async fn definition_launch_receipt_failure_rolls_back_task_trace_and_allows_exact_retry() {
    let mut f = Fixture::new();
    let config = f.config(json!(["list"]), json!([])).await;
    let saved = f.create(config).await;
    let path = format!(
        "/api/agent/definitions/{}/launch",
        saved["id"].as_str().unwrap()
    );
    let input = launch_body(1);
    {
        let app = f.app();
        let db = app.db.lock().unwrap();
        db.execute_batch("CREATE TEMP TRIGGER fail_saved_launch BEFORE INSERT ON agent_definition_launches BEGIN SELECT RAISE(ABORT,'synthetic receipt refusal'); END;").unwrap();
    }
    assert_eq!(
        f.request(Method::POST, &path, input.clone()).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(f.counts(), (0, 0, 0));
    {
        let app = f.app();
        app.db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER fail_saved_launch")
            .unwrap();
    }
    f.ok(Method::POST, &path, input).await;
    assert_eq!(f.counts(), (1, 1, 1));
}

struct ModelFixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for ModelFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn model_fixture() -> ModelFixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let server=Router::new().route("/v1/models",get(||async{Json(json!({"data":[{"id":"saved-agent-a"},{"id":"saved-agent-b"}]}))})).route("/v1/chat/completions",post(move|Json(request):Json<Value>|{let captured=captured.clone();async move{
        let mut rows=captured.lock().unwrap();rows.push(request);let reply=if rows.len()==1{json!({"content":"Inspect selected workspace","tool_call":{"name":"list_directory","arguments":{}}})}else{json!({"content":"Inspection completed.","tool_call":null})};Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":reply.to_string()}}]}))
    }}));
    let server = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    ModelFixture {
        endpoint,
        requests,
        server,
    }
}
async fn select(f: &Fixture, server: &ModelFixture, model: &str) -> (Value, Value) {
    let app = f.app();
    let config = crate::Config {
        provider: "openai".into(),
        endpoint: server.endpoint.clone(),
        model: model.into(),
    };
    let _ = crate::set_config(State(app.clone()), Json(config.clone()))
        .await
        .unwrap();
    let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":"openai","endpoint":server.endpoint});
    let _ = crate::test_connection(
        State(app.clone()),
        Json(serde_json::from_value(expected.clone()).unwrap()),
    )
    .await
    .unwrap();
    let selection = f
        .ok(Method::POST, "/api/agent/model-selection", expected.clone())
        .await;
    (expected, selection)
}

#[tokio::test]
async fn saved_persona_reaches_real_bounded_runner_with_only_captured_tools_and_target() {
    let server = model_fixture().await;
    let mut f = Fixture::new();
    let (_, other) = select(&f, &server, "saved-agent-b").await;
    let (expected, selected) = select(&f, &server, "saved-agent-a").await;
    let resources = json!([{"kind":"provider_binding","id":selected["scope_id"]},{"kind":"provider_binding","id":other["scope_id"]}]);
    let mut config = f
        .config(json!(["list", "write", "model_inference"]), resources)
        .await;
    config["capabilities"] = json!(["list", "model_inference"]);
    config["model"] = json!({"mode":"pinned","provider":"openai","endpoint":server.endpoint,"model":"saved-agent-a","scope_id":selected["scope_id"]});
    let saved = f.create(config.clone()).await;
    let mut request = launch_body(1);
    request["model_review"] = json!({"expected":expected,"binding_id":selected["binding_id"]});
    let launched = f.launch(&saved, request.clone()).await;
    let id = launched["task"]["id"].as_str().unwrap();
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(launched["model_started"], false);
    config["system_prompt"] = json!("Edited after launch");
    f.ok(
        Method::PUT,
        &format!("/api/agent/definitions/{}", saved["id"].as_str().unwrap()),
        json!({"expected_revision":1,"definition":config,"confirmed":true}),
    )
    .await;
    let (wrong_expected, _) = select(&f, &server, "saved-agent-b").await;
    assert_eq!(
        f.request(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-run"),
            json!({"expected_revision":0,"expected":wrong_expected,"max_steps":2,"confirmed":true})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(server.requests.lock().unwrap().is_empty());
    let (expected, _) = select(&f, &server, "saved-agent-a").await;
    let finished = f
        .ok(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-run"),
            json!({"expected_revision":0,"expected":expected,"max_steps":2,"confirmed":true}),
        )
        .await;
    assert_eq!(finished["task"]["state"]["kind"], "completed");
    assert_eq!(finished["task"]["model_steps"], 2);
    {
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for sent in requests.iter() {
            assert_eq!(sent["model"], "saved-agent-a");
            assert!(!sent["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(PERSONA));
            let context: Value =
                serde_json::from_str(sent["messages"][1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(
                context["task_context"]["agent_persona"]["system_prompt"],
                PERSONA
            );
            assert!(context["available_tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "list_directory"));
            assert!(!context["available_tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "write_file"));
        }
    }
    f.restart().await;
    let task = f
        .ok(Method::GET, &format!("/api/agent/tasks/{id}"), Value::Null)
        .await;
    assert_eq!(task["task"]["agent_persona"]["definition_revision"], 1);
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    f.encrypted();
}

#[tokio::test]
async fn definition_model_review_requires_explicit_current_binding_and_existing_scope() {
    let server = model_fixture().await;
    let mut f = Fixture::new();
    let (expected, selected) = select(&f, &server, "saved-agent-a").await;
    let config = f.config(json!(["model_inference"]), json!([])).await;
    let saved = f.create(config).await;
    let path = format!(
        "/api/agent/definitions/{}/launch",
        saved["id"].as_str().unwrap()
    );
    assert_eq!(
        f.request(Method::POST, &path, launch_body(1)).await.0,
        StatusCode::FORBIDDEN
    );
    let mut input = launch_body(1);
    input["model_review"] = json!({"expected":expected,"binding_id":"spoofed"});
    assert_eq!(
        f.request(Method::POST, &path, input.clone()).await.0,
        StatusCode::CONFLICT
    );
    input["model_review"]["binding_id"] = selected["binding_id"].clone();
    assert_eq!(
        f.request(Method::POST, &path, input).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(f.counts(), (0, 0, 0));
    assert!(server.requests.lock().unwrap().is_empty());
}
