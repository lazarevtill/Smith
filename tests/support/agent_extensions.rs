//! Synthetic extension workflows on a genuinely keyed profile connection.
use super::*;
use axum::{
    body::{to_bytes, Body as HttpBody},
    http::{Method, Request, StatusCode},
};
use smith_profile_storage as storage;
use tower::ServiceExt;

const SECRET: &str = "synthetic encrypted extensions passphrase";
const SENTINEL: &str = "PRIVATE_EXTENSION_INSTRUCTIONS_9237 follow the user's selected task";
struct Fixture {
    dir: tempfile::TempDir,
    app: Option<Arc<App>>,
    roots: Vec<std::path::PathBuf>,
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
            app: Some(app),
            roots: vec![],
        }
    }
    fn app(&self) -> Arc<App> {
        self.app.as_ref().unwrap().clone()
    }
    fn store(&self) -> AgentStore {
        AgentStore::new(&self.app()).unwrap()
    }
    async fn request(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(if input.is_null() {
                HttpBody::empty()
            } else {
                HttpBody::from(input.to_string())
            })
            .unwrap();
        let response = routes()
            .merge(crate::agent_api::routes())
            .merge(crate::agent_definitions::routes())
            .with_state(self.app())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn ok(&self, method: Method, path: &str, input: Value) -> Value {
        let (s, v) = self.request(method, path, input).await;
        assert_eq!(s, StatusCode::OK, "{path}: {v}");
        v
    }
    async fn create(&self, kind: &str, enabled: bool) -> Value {
        let mut definition = json!({"kind":kind,"name":format!("Synthetic {kind}"),"description":"Synthetic extension","instructions":SENTINEL});
        if kind == "plugin" {
            definition["manifest_id"] = json!("synthetic.plugin");
            definition["tools"] = json!(["run_shell"]);
        }
        if kind == "recipe" {
            definition.as_object_mut().unwrap().remove("instructions");
            definition["command"] = json!("echo {text} | wc -w");
            definition["parameters"] =
                json!({"type":"object","properties":{"text":{"type":"string"}}});
        }
        self.ok(
            Method::POST,
            "/api/agent/extensions",
            json!({"definition":definition,"enabled":enabled}),
        )
        .await["extension"]
            .clone()
    }
    fn capture(&self, selection: &InstructionSelection) -> Result<CapturedInstructions> {
        let store = self.store();
        store.with_db(|db| capture(db, store.owner().id(), selection))
    }
    async fn restart(&mut self) {
        let app = self.app.take().unwrap();
        agent_api::shutdown(&app).await.unwrap();
        app.tasks.drain().await;
        let app = Arc::try_unwrap(app).ok().expect("all requests drained");
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
        agent_api::recover(&app).unwrap();
        self.app = Some(app);
    }
    fn encrypted(&self) {
        for entry in std::fs::read_dir(self.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let bytes = std::fs::read(path).unwrap();
                assert!(!bytes.starts_with(b"SQLite format 3"));
                for text in [SECRET, SENTINEL] {
                    assert!(!bytes.windows(text.len()).any(|b| b == text.as_bytes()));
                }
            }
        }
    }
    fn seed_original(&self, row: i64, enabled: i64) {
        let payload = json!({"columns":["id","name","description","instructions","category","enabled","created_at"],"values":[{"type":"text","value":crate::id()},{"type":"text","value":"Imported skill"},{"type":"text","value":"Original preserved description"},{"type":"text","value":SENTINEL},{"type":"text","value":"research"},{"type":"integer","value":enabled.to_string()},{"type":"text","value":"2026-01-01T00:00:00Z"}]});
        self.app().db.lock().unwrap().execute("INSERT INTO legacy_records(table_name,row_number,record_id,payload_json) VALUES('skills',?1,'original-id',?2)",params![row,payload.to_string()]).unwrap();
    }
    async fn workspace(&mut self, capabilities: Value, resources: Value) -> Value {
        let created=self.ok(Method::POST,"/api/agent/workspaces",json!({"label":"Synthetic extension workspace","capabilities":capabilities,"resources":resources,"confirmed":true})).await;
        let policy = self
            .store()
            .policy(created["workspace"]["id"].as_str().unwrap())
            .unwrap();
        self.roots.push(
            smith_agent_core::Workspace::reopen_temporary(&policy.identity)
                .unwrap()
                .host_path()
                .to_owned(),
        );
        created["workspace"].clone()
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

#[tokio::test]
async fn manual_and_saved_launch_capture_enabled_defaults_and_reject_stale_snapshot_transactions() {
    use smith_agent_core::{TaskEngine, ToolRegistry};
    let mut f = Fixture::new();
    let source = f.create("skill", true).await;
    let workspace = f.workspace(json!(["list"]), json!([])).await;
    let task = f
        .ok(
            Method::POST,
            "/api/agent/tasks",
            json!({"workspace_id":workspace["id"],"policy_revision":1,"input":"Inspect"}),
        )
        .await;
    assert_eq!(task["task"]["extensions"]["selection"]["mode"], "enabled");
    assert_eq!(
        task["task"]["extensions"]["items"][0]["instructions"],
        SENTINEL
    );
    let store = f.store();
    let before = f.capture(&InstructionSelection::Enabled).unwrap();
    let path = format!("/api/agent/extensions/{}", source["id"].as_str().unwrap());
    let mut edited = source["definition"].clone();
    edited["instructions"] = json!("Edited source");
    f.ok(
        Method::PUT,
        &path,
        json!({"expected_revision":1,"definition":edited,"enabled":true}),
    )
    .await;
    let policy = store.policy(workspace["id"].as_str().unwrap()).unwrap();
    let grant = store.grant(&policy).unwrap();
    assert!(matches!(
        TaskEngine::new(store.clone(), ToolRegistry::default()).create_with_context(
            &grant,
            "Stale creation",
            None,
            before
        ),
        Err(Error::Conflict)
    ));
    let saved=f.ok(Method::POST,"/api/agent/definitions",json!({"definition":{"name":"Extension agent","system_prompt":"","soul":"","workspace_id":workspace["id"],"policy_revision":1,"capabilities":["list"],"resources":[],"model":{"mode":"selected_at_launch"}},"confirmed":true})).await;
    let launch_path = format!(
        "/api/agent/definitions/{}/launch",
        saved["definition"]["id"].as_str().unwrap()
    );
    let request =
        json!({"request_id":crate::id(),"expected_revision":1,"input":"Inspect","confirmed":true});
    let launched = f.ok(Method::POST, &launch_path, request.clone()).await;
    assert_eq!(
        launched["task"]["extensions"]["items"][0]["instructions"],
        "Edited source"
    );
    f.ok(Method::DELETE, &path, json!({"expected_revision":2}))
        .await;
    f.restart().await;
    let replay = f.ok(Method::POST, &launch_path, request).await;
    assert_eq!(replay["task"]["id"], launched["task"]["id"]);
    assert_eq!(replay["task"]["extensions"], launched["task"]["extensions"]);
    let retained = f
        .ok(
            Method::GET,
            &format!("/api/agent/tasks/{}", task["task"]["id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(
        retained["task"]["extensions"]["items"][0]["instructions"],
        SENTINEL
    );
    let task_id = retained["task"]["id"].as_str().unwrap();
    let store = f.store();
    use smith_agent_core::TraceStore;
    let mut forged = store.load(task_id).unwrap();
    forged.revision += 1;
    forged.extensions = CapturedInstructions::default();
    assert!(matches!(
        store.commit(
            Some(forged.revision - 1),
            &forged,
            &smith_agent_core::TraceEvent::Started
        ),
        Err(Error::Denied(_))
    ));
}

#[tokio::test]
async fn actual_bounded_runner_receives_full_immutable_instruction_data_and_no_extra_tools() {
    use std::sync::Mutex;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let server=Router::new().route("/v1/models",get(||async{Json(json!({"data":[{"id":"extension-fixture"}]}))})).route("/v1/chat/completions",post(move|Json(value):Json<Value>|{let captured=captured.clone();async move{let mut requests=captured.lock().unwrap();requests.push(value);let reply=if requests.len()==1{json!({"content":"Inspect granted workspace","tool_call":{"name":"list_directory","arguments":{}}})}else{json!({"content":"Done","tool_call":null})};Json(json!({"choices":[{"finish_reason":"stop","message":{"content":reply.to_string(),"role":"assistant"}}]}))}}));
    let server = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    struct Stop(tokio::task::JoinHandle<()>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop = Stop(server);
    let mut f = Fixture::new();
    let app = f.app();
    let _ = crate::set_config(
        State(app.clone()),
        Json(crate::Config {
            provider: "openai".into(),
            endpoint: endpoint.clone(),
            model: "extension-fixture".into(),
        }),
    )
    .await
    .unwrap();
    let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":"openai","endpoint":endpoint});
    let _ = crate::test_connection(
        State(app.clone()),
        Json(serde_json::from_value(expected.clone()).unwrap()),
    )
    .await
    .unwrap();
    let selected = f
        .ok(Method::POST, "/api/agent/model-selection", expected.clone())
        .await;
    let long_text = format!("{SENTINEL}\n{}\nEND_OF_FULL_CAPTURE", "x".repeat(3000));
    let extension=f.ok(Method::POST,"/api/agent/extensions",json!({"definition":{"kind":"method","name":"Full procedure","description":"No truncation or authority","instructions":long_text},"enabled":true})).await;
    let workspace = f
        .workspace(
            json!(["list", "model_inference"]),
            json!([{"kind":"provider_binding","id":selected["scope_id"]}]),
        )
        .await;
    let task = f
        .ok(
            Method::POST,
            "/api/agent/tasks",
            json!({"workspace_id":workspace["id"],"policy_revision":1,"input":"List workspace"}),
        )
        .await;
    let task_id = task["task"]["id"].as_str().unwrap();
    f.ok(
        Method::PATCH,
        &format!(
            "/api/agent/extensions/{}/enabled",
            extension["extension"]["id"].as_str().unwrap()
        ),
        json!({"expected_revision":1,"enabled":false}),
    )
    .await;
    let completed = f
        .ok(
            Method::POST,
            &format!("/api/agent/tasks/{task_id}/model-run"),
            json!({"expected_revision":0,"expected":expected,"max_steps":2,"confirmed":true}),
        )
        .await;
    assert_eq!(completed["task"]["state"]["kind"], "completed");
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(!request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(SENTINEL));
            let context: Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            assert_eq!(
                context["task_context"]["extensions"]["items"][0]["instructions"],
                long_text
            );
            assert!(!context["available_tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "write_file"));
            assert!(context["task_context"].to_string().len() <= 40 * 1024);
        }
    }
    drop(app);
    f.restart().await;
    let retained = f
        .ok(
            Method::GET,
            &format!("/api/agent/tasks/{task_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(
        retained["task"]["extensions"]["items"][0]["instructions"],
        long_text
    );
    f.encrypted();
}

#[tokio::test]
async fn extension_crud_is_versioned_encrypted_and_restore_preserves_enabled_preference() {
    let mut f = Fixture::new();
    let created = f.create("skill", true).await;
    let id = created["id"].as_str().unwrap();
    let path = format!("/api/agent/extensions/{id}");
    let body = json!({"expected_revision":1,"definition":created["definition"],"enabled":true});
    assert_eq!(
        f.ok(Method::PUT, &path, body.clone()).await["extension"]["revision"],
        1
    );
    let disabled = json!({"expected_revision":1,"enabled":false});
    let enabled_path = format!("{path}/enabled");
    let (a, b) = tokio::join!(
        f.request(Method::PATCH, &enabled_path, disabled.clone()),
        f.request(Method::PATCH, &enabled_path, disabled)
    );
    assert!([a.0, b.0].contains(&StatusCode::OK));
    assert!([a.0, b.0].contains(&StatusCode::CONFLICT));
    f.ok(
        Method::PATCH,
        &format!("{path}/enabled"),
        json!({"expected_revision":2,"enabled":true}),
    )
    .await;
    let trashed = f
        .ok(Method::DELETE, &path, json!({"expected_revision":3}))
        .await;
    assert_eq!(trashed["extension"]["enabled"], true);
    assert_eq!(trashed["extension"]["deleted"], true);
    assert!(f
        .capture(&InstructionSelection::Enabled)
        .unwrap()
        .items
        .is_empty());
    f.restart().await;
    assert_eq!(
        f.request(Method::GET, &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    let restored = f
        .ok(
            Method::POST,
            &format!("{path}/restore"),
            json!({"expected_revision":4}),
        )
        .await;
    assert_eq!(restored["extension"]["enabled"], true);
    assert_eq!(restored["extension"]["revision"], 5);
    assert_eq!(
        f.capture(&InstructionSelection::Enabled).unwrap().items[0].instructions,
        SENTINEL
    );
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM agent_tasks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.encrypted();
    let other = Fixture::new();
    assert_eq!(
        other.request(Method::GET, &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn enabled_capture_is_complete_bounded_and_recipe_metadata_has_no_execution() {
    let f = Fixture::new();
    let skill = f.create("skill", true).await;
    f.create("method", true).await;
    f.create("plugin", true).await;
    let recipe = f.create("recipe", true).await;
    f.create("skill", false).await;
    let captured = f.capture(&InstructionSelection::Enabled).unwrap();
    assert_eq!(captured.items.len(), 3);
    assert!(captured.items.iter().all(|i| i.instructions == SENTINEL));
    assert_eq!(recipe["execution_status"], "unsupported_wsl2_recipe");
    let selected = InstructionSelection::Explicit {
        references: vec![InstructionRef {
            id: skill["id"].as_str().unwrap().into(),
            revision: 1,
        }],
    };
    assert_eq!(f.capture(&selected).unwrap().items.len(), 1);
    let id = recipe["id"].as_str().unwrap();
    assert!(matches!(
        f.capture(&InstructionSelection::Explicit {
            references: vec![InstructionRef {
                id: id.into(),
                revision: 1
            }]
        }),
        Err(Error::Unsupported(_))
    ));
    for _ in 0..4 {
        f.create("skill", true).await;
    }
    assert!(matches!(
        f.capture(&InstructionSelection::Enabled),
        Err(Error::LimitExceeded(_))
    ));
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM agent_extensions WHERE enabled=1 AND deleted=0",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        8
    );
    assert_eq!(f.capture(&selected).unwrap().items.len(), 1);
}

#[tokio::test]
async fn capture_revalidation_refuses_source_edits_and_newly_enabled_items() {
    let f = Fixture::new();
    let skill = f.create("skill", true).await;
    let captured = f.capture(&InstructionSelection::Enabled).unwrap();
    let store = f.store();
    store
        .with_db(|db| validate_capture(db, store.owner().id(), &captured))
        .unwrap();
    f.create("method", true).await;
    assert!(matches!(
        store.with_db(|db| validate_capture(db, store.owner().id(), &captured)),
        Err(Error::Conflict)
    ));
    let selected = f
        .capture(&InstructionSelection::Explicit {
            references: vec![InstructionRef {
                id: skill["id"].as_str().unwrap().into(),
                revision: 1,
            }],
        })
        .unwrap();
    let path = format!("/api/agent/extensions/{}", skill["id"].as_str().unwrap());
    let mut edited = skill["definition"].clone();
    edited["instructions"] = json!("Edited later");
    f.ok(
        Method::PUT,
        &path,
        json!({"expected_revision":1,"definition":edited,"enabled":true}),
    )
    .await;
    assert!(matches!(
        store.with_db(|db| validate_capture(db, store.owner().id(), &selected)),
        Err(Error::Conflict)
    ));
    assert_eq!(selected.items[0].instructions, SENTINEL);
}

#[tokio::test]
async fn original_import_is_disabled_exact_once_and_receipt_survives_purge_restart() {
    let mut f = Fixture::new();
    f.seed_original(1, 1);
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/import-review",
            json!({"source_table":"skills","row_number":1}),
        )
        .await;
    let input = json!({"request_id":crate::id(),"source_table":"skills","row_number":1,"expected_source_sha256":review["source_sha256"]});
    let imported = f
        .ok(Method::POST, "/api/agent/extensions/import", input.clone())
        .await;
    let extension = &imported["extension"];
    assert_eq!(extension["enabled"], false);
    assert_eq!(extension["provenance"]["original_enabled"], true);
    assert!(f
        .capture(&InstructionSelection::Enabled)
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        f.ok(Method::POST, "/api/agent/extensions/import", input.clone())
            .await["replayed"],
        true
    );
    let path = format!(
        "/api/agent/extensions/{}",
        extension["id"].as_str().unwrap()
    );
    f.ok(Method::DELETE, &path, json!({"expected_revision":1}))
        .await;
    f.ok(
        Method::POST,
        &format!("{path}/purge"),
        json!({"expected_revision":2,"confirm_id":extension["id"],"permanent":true}),
    )
    .await;
    f.restart().await;
    let replay = f
        .ok(Method::POST, "/api/agent/extensions/import", input.clone())
        .await;
    assert_eq!(replay["extension"]["id"], extension["id"]);
    assert_eq!(replay["replayed"], true);
    let mut changed = input;
    changed["expected_source_sha256"] = json!("0".repeat(64));
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/import", changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM legacy_records WHERE table_name='skills'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM agent_extensions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.encrypted();
}

#[tokio::test]
async fn import_failure_rolls_back_definition_and_changed_or_invalid_source_is_not_silently_reinterpreted(
) {
    let f = Fixture::new();
    f.seed_original(1, 1);
    f.seed_original(2, 7);
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/import-review",
            json!({"source_table":"skills","row_number":2})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/import-review",
            json!({"source_table":"skills","row_number":1}),
        )
        .await;
    let input = json!({"request_id":crate::id(),"source_table":"skills","row_number":1,"expected_source_sha256":review["source_sha256"]});
    f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER fail_extension_receipt BEFORE INSERT ON agent_extension_imports BEGIN SELECT RAISE(ABORT,'synthetic'); END;").unwrap();
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/import", input.clone())
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM agent_extensions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.app()
        .db
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_extension_receipt")
        .unwrap();
    f.ok(Method::POST, "/api/agent/extensions/import", input)
        .await;
    let mut changed = json!({"request_id":crate::id(),"source_table":"skills","row_number":1,"expected_source_sha256":"0".repeat(64)});
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/import",
            changed.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    changed["source_table"] = json!("agent_workspaces");
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/import", changed)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn strict_metadata_bounds_and_pagination_do_not_read_instruction_paths_or_activate_tools() {
    let f = Fixture::new();
    let plugin = json!({"kind":"plugin","manifest_id":"selected.plugin","name":"Selected plugin","description":"No file access","instructions_file":"/etc/passwd"});
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions",
            json!({"definition":plugin,"enabled":true})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut plugin = plugin;
    plugin["instructions"] = json!("Inline literal content");
    plugin["tools"] = json!(["run_shell", "send_webhook"]);
    f.ok(
        Method::POST,
        "/api/agent/extensions",
        json!({"definition":plugin,"enabled":true}),
    )
    .await;
    assert_eq!(
        f.capture(&InstructionSelection::Enabled).unwrap().items[0].instructions,
        "Inline literal content"
    );
    for _ in 0..33 {
        f.create("method", false).await;
    }
    let page = f
        .ok(
            Method::GET,
            "/api/agent/extensions?kind=method",
            Value::Null,
        )
        .await;
    assert_eq!(page["extensions"].as_array().unwrap().len(), 32);
    assert!(page["extensions"][0].get("definition").is_none());
    let next = f
        .ok(
            Method::GET,
            &format!(
                "/api/agent/extensions?kind=method&after={}",
                page["next_after"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
    assert_eq!(next["extensions"].as_array().unwrap().len(), 1);
    let oversized =
        json!({"kind":"skill","name":"Too large","description":"","instructions":"a".repeat(8193)});
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions",
            json!({"definition":oversized,"enabled":true})
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM agent_tasks", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[cfg(unix)]
fn plugin_source() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("chosen-plugin");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("plugin.json"),json!({"id":"synthetic.selected","name":"Selected plugin","instructions_file":"SKILL.md","tools":["run_shell"],"homepage":"https://invalid.example/not-fetched"}).to_string()).unwrap();
    std::fs::write(path.join("SKILL.md"), SENTINEL).unwrap();
    (dir, path)
}

#[cfg(unix)]
#[tokio::test]
async fn selected_plugin_is_disabled_until_enable_and_captured_without_external_authority() {
    let mut f = Fixture::new();
    let (_source, path) = plugin_source();
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path}),
        )
        .await;
    assert_eq!(review["definition"]["instructions"], SENTINEL);
    assert_eq!(
        review["provenance"]["source"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(review["enabled_on_import"], false);
    let request = json!({"request_id":crate::id(),"review_id":review["review_id"],"expected_source_sha256":review["source_sha256"]});
    let imported = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone(),
        )
        .await;
    assert_eq!(imported["extension"]["enabled"], false);
    assert!(f
        .capture(&InstructionSelection::Enabled)
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        imported["extension"]["provenance"]["kind"],
        "selected_directory"
    );
    let extension = imported["extension"]["id"].as_str().unwrap();
    f.ok(
        Method::PATCH,
        &format!("/api/agent/extensions/{extension}/enabled"),
        json!({"expected_revision":1,"enabled":true}),
    )
    .await;
    let workspace = f.workspace(json!(["list"]), json!([])).await;
    let task = f
        .ok(
            Method::POST,
            "/api/agent/tasks",
            json!({"workspace_id":workspace["id"],"policy_revision":1,"input":"Inspect"}),
        )
        .await;
    assert_eq!(task["task"]["capabilities"], json!(["list"]));
    assert_eq!(
        task["task"]["extensions"]["items"][0]["instructions"],
        SENTINEL
    );
    assert_eq!(
        std::fs::read_to_string(path.join("SKILL.md")).unwrap(),
        SENTINEL
    );
    std::fs::remove_dir_all(&path).unwrap();
    f.restart().await;
    let retained = f
        .ok(
            Method::GET,
            &format!("/api/agent/tasks/{}", task["task"]["id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
    assert_eq!(
        retained["task"]["extensions"]["items"][0]["instructions"],
        SENTINEL
    );
    let replay = f
        .ok(Method::POST, "/api/agent/extensions/plugin-import", request)
        .await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["extension"]["id"], extension);
    f.encrypted();
}

#[cfg(unix)]
#[tokio::test]
async fn selected_plugin_exact_concurrent_retry_survives_purge_and_missing_source() {
    let mut f = Fixture::new();
    let (_source, path) = plugin_source();
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path}),
        )
        .await;
    let request = json!({"request_id":crate::id(),"review_id":review["review_id"],"expected_source_sha256":review["source_sha256"]});
    let (a, b) = tokio::join!(
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone()
        ),
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone()
        )
    );
    assert_eq!(a.0, StatusCode::OK, "{:?}", a);
    assert_eq!(b.0, StatusCode::OK, "{:?}", b);
    assert_eq!(a.1["extension"]["id"], b.1["extension"]["id"]);
    assert_ne!(a.1["replayed"], b.1["replayed"]);
    let id = a.1["extension"]["id"].as_str().unwrap();
    f.ok(
        Method::DELETE,
        &format!("/api/agent/extensions/{id}"),
        json!({"expected_revision":1}),
    )
    .await;
    f.ok(
        Method::POST,
        &format!("/api/agent/extensions/{id}/purge"),
        json!({"expected_revision":2,"confirm_id":id,"permanent":true}),
    )
    .await;
    std::fs::remove_dir_all(path).unwrap();
    f.restart().await;
    let replay = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone(),
        )
        .await;
    assert_eq!(replay["extension"]["id"], id);
    assert_eq!(replay["replayed"], true);
    assert!(f
        .ok(
            Method::GET,
            "/api/agent/extensions?include_deleted=true",
            Value::Null
        )
        .await["extensions"]
        .as_array()
        .unwrap()
        .is_empty());
    let mut changed = request.clone();
    changed["review_id"] = json!(crate::id());
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/plugin-import", changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.seed_original(1, 1);
    let legacy = f
        .ok(
            Method::POST,
            "/api/agent/extensions/import-review",
            json!({"source_table":"skills","row_number":1}),
        )
        .await;
    assert_eq!(f.request(Method::POST,"/api/agent/extensions/import",json!({"request_id":request["request_id"],"source_table":"skills","row_number":1,"expected_source_sha256":legacy["source_sha256"]})).await.0,StatusCode::CONFLICT);
    let counts:(i64,i64)=f.app().db.lock().unwrap().query_row("SELECT (SELECT count(*) FROM agent_extensions),(SELECT count(*) FROM agent_extension_imports)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(counts, (0, 1));
    f.encrypted();
}

#[cfg(unix)]
#[tokio::test]
async fn selected_plugin_refuses_changed_source_wrong_profile_and_unretained_reviews() {
    let mut f = Fixture::new();
    let other = Fixture::new();
    let (_source, path) = plugin_source();
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path}),
        )
        .await;
    let request = json!({"request_id":crate::id(),"review_id":review["review_id"],"expected_source_sha256":review["source_sha256"]});
    assert_eq!(
        other
            .request(
                Method::POST,
                "/api/agent/extensions/plugin-import",
                request.clone()
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let mut wrong = request.clone();
    wrong["expected_source_sha256"] = json!("0".repeat(64));
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/plugin-import", wrong)
            .await
            .0,
        StatusCode::CONFLICT
    );
    std::fs::write(path.join("SKILL.md"), "Changed after review").unwrap();
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(f
        .ok(Method::GET, "/api/agent/extensions", Value::Null)
        .await["extensions"]
        .as_array()
        .unwrap()
        .is_empty());
    let fresh = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path}),
        )
        .await;
    let fresh_request = json!({"request_id":crate::id(),"review_id":fresh["review_id"],"expected_source_sha256":fresh["source_sha256"]});
    f.restart().await;
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            fresh_request
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path,"enabled":true})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[cfg(unix)]
#[tokio::test]
async fn selected_plugin_receipt_failure_is_atomic_and_one_review_binds_one_request() {
    let f = Fixture::new();
    let (_source, path) = plugin_source();
    let review = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-review",
            json!({"directory":path}),
        )
        .await;
    let request = json!({"request_id":crate::id(),"review_id":review["review_id"],"expected_source_sha256":review["source_sha256"]});
    f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER refuse_plugin_receipt BEFORE INSERT ON agent_extension_imports BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
    assert_eq!(
        f.request(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone()
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(f
        .ok(Method::GET, "/api/agent/extensions", Value::Null)
        .await["extensions"]
        .as_array()
        .unwrap()
        .is_empty());
    let mut another = request.clone();
    another["request_id"] = json!(crate::id());
    assert_eq!(
        f.request(Method::POST, "/api/agent/extensions/plugin-import", another)
            .await
            .0,
        StatusCode::CONFLICT
    );
    f.app()
        .db
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_plugin_receipt;")
        .unwrap();
    let imported = f
        .ok(
            Method::POST,
            "/api/agent/extensions/plugin-import",
            request.clone(),
        )
        .await;
    assert_eq!(imported["replayed"], false);
    let replay = f
        .ok(Method::POST, "/api/agent/extensions/plugin-import", request)
        .await;
    assert_eq!(replay["extension"]["id"], imported["extension"]["id"]);
    assert_eq!(replay["replayed"], true);
}
