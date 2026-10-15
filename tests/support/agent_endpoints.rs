//! Actual HTTP router contracts over synthetic, genuinely keyed SQLCipher files.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request},
};
use smith_agent_core::{ExecutionGrant, TraceEvent, TraceRecord};
use smith_profile_storage as storage;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use tower::ServiceExt;

const SECRET: &str = "synthetic agent endpoint profile passphrase";
const SENTINEL: &str = "PRIVATE_AGENT_PAYLOAD_83d7e67c";
struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    app: Option<Arc<App>>,
    roots: Vec<PathBuf>,
}
impl Fixture {
    async fn setup_scoped(&mut self, caps: Value, resources: Value) -> (String, Value) {
        let created=self.post("/api/agent/workspaces",json!({"label":"Scoped synthetic task","capabilities":caps,"resources":resources,"confirmed":true})).await;
        let workspace = created["workspace"]["id"].as_str().unwrap().to_owned();
        let store = AgentStore::new(&self.app()).unwrap();
        let policy = store.policy(&workspace).unwrap();
        self.roots.push(
            Workspace::reopen_temporary(&policy.identity)
                .unwrap()
                .host_path()
                .to_path_buf(),
        );
        let task = self
            .post(
                "/api/agent/tasks",
                json!({"workspace_id":workspace,"policy_revision":1,"input":SENTINEL}),
            )
            .await;
        let id = task["task"]["id"].as_str().unwrap().to_owned();
        let task = self
            .post(
                &format!("/api/agent/tasks/{id}/start"),
                json!({"expected_revision":0}),
            )
            .await;
        (id, task)
    }
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join(storage::DB_FILE);
        let db = storage::create(&path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let app = crate::app_from_connection(db).unwrap();
        recover(&app).unwrap();
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
    async fn call(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
        request(self.app(), method, path, input).await
    }
    async fn post(&self, path: &str, input: Value) -> Value {
        let (status, body) = self.call(Method::POST, path, input).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }
    async fn get(&self, path: &str) -> Value {
        let (status, body) = self.call(Method::GET, path, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        body
    }
    async fn setup(&mut self, caps: Value) -> (String, Value) {
        let created = self
            .post(
                "/api/agent/workspaces",
                json!({"label":"Synthetic workspace","capabilities":caps,"confirmed":true}),
            )
            .await;
        let workspace = created["workspace"]["id"].as_str().unwrap().to_owned();
        let store = AgentStore::new(&self.app()).unwrap();
        let policy = store.policy(&workspace).unwrap();
        self.roots.push(
            Workspace::reopen_temporary(&policy.identity)
                .unwrap()
                .host_path()
                .to_path_buf(),
        );
        let task = self
            .post(
                "/api/agent/tasks",
                json!({"workspace_id":workspace,"policy_revision":1,"input":SENTINEL}),
            )
            .await;
        let id = task["task"]["id"].as_str().unwrap().to_owned();
        let running = self
            .post(
                &format!("/api/agent/tasks/{id}/start"),
                json!({"expected_revision":0}),
            )
            .await;
        (id, running)
    }
    async fn action(&self, id: &str, current: &Value, action: Value) -> Value {
        self.post(&format!("/api/agent/tasks/{id}/actions"),json!({"expected_revision":current["task"]["revision"],"request_id":crate::id(),"action":action})).await
    }
    async fn approve(&self, id: &str, current: &Value) -> Value {
        self.post(&format!("/api/agent/tasks/{id}/approval"),json!({"expected_revision":current["task"]["revision"],"call_id":current["task"]["state"]["call"]["id"],"decision":"approve"})).await
    }
    async fn restart(&mut self) {
        let app = self.app.take().unwrap();
        shutdown(&app).await.unwrap();
        app.tasks.drain().await;
        let app = Arc::try_unwrap(app).ok().expect("all app leases drained");
        storage::close(app.db.into_inner().unwrap()).unwrap();
        let db =
            storage::unlock(&self.path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let app = crate::app_from_connection(db).unwrap();
        recover(&app).unwrap();
        self.app = Some(app);
    }
    fn assert_encrypted(&self) {
        for entry in std::fs::read_dir(self.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let bytes = std::fs::read(path).unwrap();
            for secret in [
                SENTINEL.as_bytes(),
                SECRET.as_bytes(),
                b"Synthetic workspace",
                b"expected_sha256",
                b"needs_approval",
            ] {
                assert!(
                    !bytes.windows(secret.len()).any(|v| v == secret),
                    "plaintext sentinel leaked into profile file"
                );
            }
            assert!(!bytes.starts_with(b"SQLite format 3\0"));
        }
    }
}

fn seed_chats(f: &Fixture) -> (String, String) {
    let selected = crate::id();
    let other = crate::id();
    let app = f.app();
    let db = app.db.lock().unwrap();
    crate::chat_management::create(&db, &selected, "Selected history", 1).unwrap();
    crate::chat_management::create(&db, &other, "Hidden history", 1).unwrap();
    for (chat, content) in [
        (&selected, "Selected literal [x].* proof"),
        (&other, "PRIVATE OTHER CONVERSATION [x].*"),
    ] {
        db.execute("INSERT INTO messages(id,chat_id,role,content,status,created_at) VALUES(?1,?2,'user',?3,'complete',1)",rusqlite::params![crate::id(),chat,content]).unwrap();
    }
    (selected, other)
}
async fn tool(f: &Fixture, id: &str, task: &Value, name: &str, args: Value) -> Value {
    f.action(
        id,
        task,
        json!({"kind":"tool","name":name,"arguments":args}),
    )
    .await
}

#[tokio::test]
async fn domain_conversation_scopes_export_and_chart_are_real_and_bounded() {
    let mut f = Fixture::new();
    let (selected, other) = seed_chats(&f);
    let (id, mut task) = f
        .setup_scoped(
            json!(["conversations", "export_conversations", "charts", "status"]),
            json!([{"kind":"conversation","id":selected}]),
        )
        .await;
    task = tool(&f, &id, &task, "list_conversations", json!({})).await;
    assert_eq!(
        task["task"]["last_result"]["conversations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        task["task"]["last_result"]["conversations"][0]["id"],
        selected
    );
    task = tool(
        &f,
        &id,
        &task,
        "search_conversations",
        json!({"query":"[x].*"}),
    )
    .await;
    assert_eq!(
        task["task"]["last_result"]["matches"][0]["session_id"],
        selected
    );
    task = tool(
        &f,
        &id,
        &task,
        "read_conversation",
        json!({"session_id":selected}),
    )
    .await;
    assert!(task["task"]["last_result"]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Selected literal"));
    task = tool(
        &f,
        &id,
        &task,
        "export_conversation",
        json!({"session_id":selected,"format":"markdown"}),
    )
    .await;
    assert!(task["task"]["last_result"]["content"]
        .as_str()
        .unwrap()
        .contains("Selected literal"));
    assert!(!task.to_string().contains("PRIVATE OTHER"));
    let denied=f.call(Method::POST,&format!("/api/agent/tasks/{id}/actions"),json!({"expected_revision":task["task"]["revision"],"request_id":crate::id(),"action":{"kind":"tool","name":"read_conversation","arguments":{"session_id":other}}})).await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    task = f.get(&format!("/api/agent/tasks/{id}")).await;
    task=tool(&f,&id,&task,"render_chart",json!({"kind":"bar","title":"<script>alert(1)</script>","values":[-5,0,10],"labels":["a","<&\"","c"]})).await;
    let svg = task["task"]["last_result"]["svg"].as_str().unwrap();
    assert!(svg.starts_with("<svg"));
    assert!(svg.contains("&lt;script&gt;"));
    assert!(!svg.contains("<script>"));
    assert!(!svg.contains("height=\"-"));
    task = tool(&f, &id, &task, "smith_status", json!({})).await;
    assert_eq!(
        task["task"]["last_result"]["storage"],
        "encrypted_sqlcipher"
    );
    assert!(task["task"]["last_result"].get("endpoint").is_none());
}

#[tokio::test]
async fn domain_memory_and_artifact_approval_survive_restart_and_do_not_grant_new_scopes() {
    let mut f = Fixture::new();
    let(id,task)=f.setup_scoped(json!(["artifacts","save_memory","recall_memory"]),json!([{"kind":"artifact_collection","id":"unowned"},{"kind":"memory_namespace","id":"facts"}])).await;
    let pending = tool(
        &f,
        &id,
        &task,
        "save_memory",
        json!({"namespace":"facts","fact":SENTINEL,"tags":["synthetic"]}),
    )
    .await;
    assert_eq!(pending["outcome"]["kind"], "needs_approval");
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM agent_memories", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    f.restart().await;
    let pending = f.get(&format!("/api/agent/tasks/{id}")).await;
    let saved = f.approve(&id, &pending).await;
    let recalled = tool(
        &f,
        &id,
        &saved,
        "recall_memory",
        json!({"namespace":"facts","query":"private_agent"}),
    )
    .await;
    assert_eq!(
        recalled["task"]["last_result"]["memories"][0]["fact"],
        SENTINEL
    );
    let memory = recalled["task"]["last_result"]["memories"][0]["id"]
        .as_str()
        .unwrap();
    let memory_route = format!("/api/memory/{memory}");
    let (status, trashed) = f
        .call(
            Method::PATCH,
            &memory_route,
            json!({"action":"trash","version":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(trashed["version"], 2);
    let hidden = tool(
        &f,
        &id,
        &recalled,
        "recall_memory",
        json!({"namespace":"facts","query":"private_agent"}),
    )
    .await;
    assert_eq!(hidden["task"]["last_result"]["memories"], json!([]));
    assert_eq!(
        f.call(
            Method::PATCH,
            &memory_route,
            json!({"action":"restore","version":1})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.call(
            Method::PATCH,
            &memory_route,
            json!({"action":"restore","version":2})
        )
        .await
        .0,
        StatusCode::OK
    );
    let recalled = tool(
        &f,
        &id,
        &hidden,
        "recall_memory",
        json!({"namespace":"facts","query":"private_agent"}),
    )
    .await;
    assert_eq!(
        recalled["task"]["last_result"]["memories"][0]["fact"],
        SENTINEL
    );
    assert_eq!(recalled["task"]["last_result"]["memories"][0]["version"], 3);
    let pending = tool(
        &f,
        &id,
        &recalled,
        "save_artifact",
        json!({"title":"Synthetic saved artifact","content":SENTINEL}),
    )
    .await;
    let saved = f.approve(&id, &pending).await;
    let artifact = saved["task"]["last_result"]["artifact_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT content FROM artifacts WHERE id=?1",
                [artifact],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        SENTINEL
    );
    let denied=f.call(Method::POST,&format!("/api/agent/tasks/{id}/actions"),json!({"expected_revision":saved["task"]["revision"],"request_id":crate::id(),"action":{"kind":"tool","name":"recall_memory","arguments":{"namespace":"another","query":"private"}}})).await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    f.assert_encrypted();
}

#[tokio::test]
async fn domain_effect_receipt_prevents_duplicate_publication_after_completion_trace_failure() {
    let mut f = Fixture::new();
    let (id, task) = f
        .setup_scoped(
            json!(["artifacts"]),
            json!([{"kind":"artifact_collection","id":"unowned"}]),
        )
        .await;
    let pending = tool(
        &f,
        &id,
        &task,
        "save_artifact",
        json!({"title":"One publication","content":SENTINEL}),
    )
    .await;
    f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER reject_completion BEFORE INSERT ON agent_task_trace WHEN NEW.event LIKE '%tool_finished%' BEGIN SELECT RAISE(ABORT,'injected trace failure'); END;").unwrap();
    let failure=f.call(Method::POST,&format!("/api/agent/tasks/{id}/approval"),json!({"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"decision":"approve"})).await;
    assert_eq!(failure.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM artifacts WHERE name='One publication'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    f.restart().await;
    let current = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(current["task"]["state"]["kind"], "interrupted");
    let pending = f
        .post(
            &format!("/api/agent/tasks/{id}/resume"),
            json!({"expected_revision":current["task"]["revision"],"kind":"retry_uncertain_call"}),
        )
        .await;
    f.approve(&id, &pending).await;
    assert_eq!(
        f.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM artifacts WHERE name='One publication'",
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
            .query_row(
                "SELECT count(*) FROM agent_tool_receipts WHERE task_id=?1",
                [id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn document_tool_publishes_real_csv_but_result_does_not_automatically_grant_read() {
    let mut f = Fixture::new();
    let (id, task) = f
        .setup_scoped(
            json!(["write_document", "read_document"]),
            json!([{"kind":"artifact_collection","id":"unowned"}]),
        )
        .await;
    let pending=tool(&f,&id,&task,"write_document",json!({"name":"synthetic.csv","format":"csv","table":{"headers":["name","count"],"rows":[[{"type":"text","value":"example"},{"type":"number","value":"3"}]]}})).await;
    let saved = f.approve(&id, &pending).await;
    assert_ne!(saved["outcome"]["kind"], "failed", "{saved}");
    let (media_id, bytes) = {
        let app = f.app();
        let db = app.db.lock().unwrap();
        let result: (String, Vec<u8>) = db
            .query_row(
                "SELECT id,bytes FROM media_objects WHERE origin='generated'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        result
    };
    assert!(String::from_utf8(bytes).unwrap().contains("example"));
    let denied=f.call(Method::POST,&format!("/api/agent/tasks/{id}/actions"),json!({"expected_revision":saved["task"]["revision"],"request_id":crate::id(),"action":{"kind":"tool","name":"read_document","arguments":{"media_id":media_id}}})).await;
    assert_eq!(denied.0, StatusCode::FORBIDDEN);
    let (second, task) = f
        .setup_scoped(
            json!(["read_document"]),
            json!([{"kind":"document","id":media_id}]),
        )
        .await;
    let read = tool(
        &f,
        &second,
        &task,
        "read_document",
        json!({"media_id":media_id}),
    )
    .await;
    assert!(read["task"]["last_result"].to_string().contains("example"));
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for root in &self.roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
async fn request(app: Arc<App>, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:18471")
        .header("origin", "http://127.0.0.1:18471")
        .header("content-type", "application/json")
        .body(if input.is_null() {
            Body::empty()
        } else {
            Body::from(input.to_string())
        })
        .unwrap();
    let response = routes()
        .merge(crate::memory_api::routes())
        .with_state(app)
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"non_json":String::from_utf8_lossy(&bytes)})),
    )
}
fn all_caps() -> Value {
    json!(["read", "list", "search", "write", "edit", "control"])
}

#[tokio::test]
async fn encrypted_routes_roundtrip_write_search_guarded_edit_and_restart_approval() {
    let mut f = Fixture::new();
    let (id, running) = f.setup(all_caps()).await;
    let pending = f
        .action(
            &id,
            &running,
            json!({"kind":"write","path":"note.txt","contents":SENTINEL}),
        )
        .await;
    assert_eq!(pending["outcome"]["kind"], "needs_approval");
    assert!(!f.roots[0].join("note.txt").exists());
    f.assert_encrypted();
    f.restart().await;
    let pending = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(pending["task"]["state"]["kind"], "needs_approval");
    assert!(!f.roots[0].join("note.txt").exists());
    let written = f.approve(&id, &pending).await;
    let read = f
        .action(&id, &written, json!({"kind":"read","path":"note.txt"}))
        .await;
    assert_eq!(read["task"]["last_result"]["contents"], SENTINEL);
    let hash = read["task"]["last_result"]["sha256"].clone();
    let searched = f
        .action(&id, &read, json!({"kind":"search","query":"PAYLOAD"}))
        .await;
    assert_eq!(
        searched["task"]["last_result"]["matches"],
        json!(["note.txt"])
    );
    let edit=f.action(&id,&searched,json!({"kind":"edit","path":"note.txt","old_string":"PAYLOAD","new_string":"EDITED","expected_sha256":hash})).await;
    f.restart().await;
    let edited = f.approve(&id, &edit).await;
    assert_eq!(edited["task"]["last_result"]["before_sha256"], hash);
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("note.txt")).unwrap(),
        SENTINEL.replace("PAYLOAD", "EDITED")
    );
    let complete = f
        .action(
            &id,
            &edited,
            json!({"kind":"complete","summary":"Reviewed file workflow complete"}),
        )
        .await;
    assert_eq!(complete["task"]["state"]["kind"], "completed");
    let trace = f
        .get(&format!("/api/agent/tasks/{id}/trace?limit=100"))
        .await;
    assert!(trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"]["result"]["Ok"]["before_sha256"] == hash));
    f.assert_encrypted();
    assert!(storage::unlock(
        &f.path,
        &storage::Secret::new("wrong synthetic passphrase".into()).unwrap()
    )
    .is_err());
}

#[tokio::test]
async fn stale_edits_duplicate_approvals_and_cancel_preserve_complete_bytes() {
    let mut f = Fixture::new();
    let (id, running) = f.setup(all_caps()).await;
    let pending = f
        .action(
            &id,
            &running,
            json!({"kind":"write","path":"note","contents":"original"}),
        )
        .await;
    let approval = json!({"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"decision":"approve"});
    let path = format!("/api/agent/tasks/{id}/approval");
    let (first, second) = tokio::join!(
        f.call(Method::POST, &path, approval.clone()),
        f.call(Method::POST, &path, approval)
    );
    assert_eq!(
        [first.0, second.0]
            .into_iter()
            .filter(|s| *s == StatusCode::OK)
            .count(),
        1
    );
    assert_eq!(
        [first.0, second.0]
            .into_iter()
            .filter(|s| *s == StatusCode::CONFLICT)
            .count(),
        1
    );
    let current = f.get(&format!("/api/agent/tasks/{id}")).await;
    let read = f
        .action(&id, &current, json!({"kind":"read","path":"note"}))
        .await;
    let edit=f.action(&id,&read,json!({"kind":"edit","path":"note","old_string":"original","new_string":"replacement","expected_sha256":read["task"]["last_result"]["sha256"]})).await;
    std::fs::write(f.roots[0].join("note"), "independent new complete content").unwrap();
    let conflict = f.approve(&id, &edit).await;
    assert_eq!(conflict["outcome"]["kind"], "failed");
    assert_eq!(conflict["outcome"]["value"]["kind"], "conflict");
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("note")).unwrap(),
        "independent new complete content"
    );
    let cancel_pending = f
        .action(
            &id,
            &conflict,
            json!({"kind":"write","path":"note","contents":"must never replace"}),
        )
        .await;
    let cancelled=f.post(&format!("/api/agent/tasks/{id}/cancel"),json!({"expected_revision":cancel_pending["task"]["revision"],"reason":"User cancelled"})).await;
    assert_eq!(cancelled["task"]["state"]["kind"], "cancelled");
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("note")).unwrap(),
        "independent new complete content"
    );
    assert_eq!(f.call(Method::POST,&path,json!({"expected_revision":cancel_pending["task"]["revision"],"call_id":cancel_pending["task"]["state"]["call"]["id"],"decision":"approve"})).await.0,StatusCode::CONFLICT);
}

#[tokio::test]
async fn route_authority_is_explicit_strict_and_isolated_between_profiles() {
    let mut f = Fixture::new();
    for bad in [
        json!({"label":"unconfirmed","capabilities":["write"],"confirmed":false}),
        json!({"label":"unknown","capabilities":["mcp"],"confirmed":true}),
        json!({"label":"external","capabilities":["write"],"confirmed":true,"path":"/tmp"}),
    ] {
        assert!(!f
            .call(Method::POST, "/api/agent/workspaces", bad)
            .await
            .0
            .is_success());
    }
    assert_eq!(
        f.get("/api/agent/workspaces").await["workspaces"],
        json!([])
    );
    let (id, running) = f.setup(json!(["read", "list"])).await;
    let base = json!({"expected_revision":running["task"]["revision"],"request_id":crate::id(),"action":{"kind":"write","path":"x","contents":"forbidden"}});
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/actions"),
            base.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for field in ["principal", "persona", "capabilities"] {
        let mut spoof = base.clone();
        spoof[field] = json!("admin");
        assert_eq!(
            f.call(
                Method::POST,
                &format!("/api/agent/tasks/{id}/actions"),
                spoof
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(!f.roots[0].join("x").exists());
    let other = Fixture::new();
    assert_eq!(
        other
            .call(Method::GET, &format!("/api/agent/tasks/{id}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        other
            .call(
                Method::GET,
                &format!("/api/agent/tasks/{id}/trace"),
                Value::Null
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn missing_workspace_preserves_status_trace_and_owner_cancellation_after_restart() {
    let mut f = Fixture::new();
    let (id, current) = f.setup(all_caps()).await;
    let pending = f
        .action(
            &id,
            &current,
            json!({"kind":"write","path":"never","contents":"must not exist"}),
        )
        .await;
    std::fs::remove_dir(&f.roots[0]).unwrap();
    f.restart().await;
    let viewed = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(viewed["workspace"]["available"], false);
    assert_eq!(viewed["task"]["state"], pending["task"]["state"]);
    assert!(
        !f.get(&format!("/api/agent/tasks/{id}/trace")).await["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let cancelled = f
        .post(
            &format!("/api/agent/tasks/{id}/cancel"),
            json!({"expected_revision":viewed["task"]["revision"],"reason":"Workspace expired"}),
        )
        .await;
    assert_eq!(cancelled["task"]["state"]["kind"], "cancelled");
    assert!(!f.roots[0].exists());
}

struct LoseCompletion {
    inner: AgentStore,
    once: AtomicBool,
}
impl TraceStore for LoseCompletion {
    fn load(&self, id: &str) -> smith_agent_core::Result<Task> {
        self.inner.load(id)
    }
    fn events(&self, id: &str) -> smith_agent_core::Result<Vec<TraceRecord>> {
        self.inner.events(id)
    }
    fn commit(
        &self,
        expected: Option<u64>,
        task: &Task,
        event: &TraceEvent,
    ) -> smith_agent_core::Result<()> {
        if matches!(event, TraceEvent::ToolFinished { .. })
            && self.once.swap(false, Ordering::SeqCst)
        {
            return Err(Error::Persistence("synthetic lost completion".into()));
        }
        self.inner.commit(expected, task, event)
    }
}
#[tokio::test]
async fn keyed_restart_recovers_uncertain_effect_without_automatic_replay() {
    let mut f = Fixture::new();
    let (id, running) = f.setup(all_caps()).await;
    let store = AgentStore::new(&f.app()).unwrap();
    let grant: ExecutionGrant = store.grant(&store.policy_for_task(&id).unwrap()).unwrap();
    let engine = TaskEngine::new(
        LoseCompletion {
            inner: store.clone(),
            once: AtomicBool::new(true),
        },
        ToolRegistry::default(),
    );
    let call = ToolCall::new("write_file", json!({"path":"once","contents":SENTINEL}));
    engine
        .call_at(
            &id,
            running["task"]["revision"].as_u64().unwrap(),
            &grant,
            call.clone(),
        )
        .unwrap();
    let waiting = engine.get(&id, &grant).unwrap();
    assert!(matches!(
        engine.decide_approval(&id, waiting.revision, &grant, ApprovalDecision::Approve),
        Err(Error::Persistence(_))
    ));
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("once")).unwrap(),
        SENTINEL
    );
    f.restart().await;
    assert!(
        store.load(&id).is_err(),
        "weak store must not keep closed App alive"
    );
    let interrupted = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(interrupted["task"]["state"]["kind"], "interrupted");
    assert_eq!(
        interrupted["task"]["state"]["uncertain_call"]["id"],
        call.id
    );
    std::fs::write(f.roots[0].join("once"), "preserve independent contents").unwrap();
    let resumed=f.post(&format!("/api/agent/tasks/{id}/resume"),json!({"expected_revision":interrupted["task"]["revision"],"kind":"continue_without_replay"})).await;
    assert_eq!(resumed["outcome"]["value"]["replayed"], false);
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("once")).unwrap(),
        "preserve independent contents"
    );
    f.assert_encrypted();
}

#[tokio::test]
async fn shutdown_tracks_dropped_http_future_cancels_then_joins_blocking_work() {
    let mut f = Fixture::new();
    let (id, current) = f.setup(all_caps()).await;
    let app = f.app();
    let worker_app = app.clone();
    let worker_id = id.clone();
    let revision = current["task"]["revision"].as_u64().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let barrier = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let worker_barrier = barrier.clone();
    let response = tokio::spawn(async move {
        work(worker_app, Some(worker_id.clone()), move |app| {
            let _ = started_tx.send(());
            let (lock, wake) = &*worker_barrier;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = wake.wait(ready).unwrap();
            }
            drop(ready);
            let store = AgentStore::new(&app)?;
            let grant = store.grant(&store.policy_for_task(&worker_id)?)?;
            TaskEngine::new(store, ToolRegistry::default())
                .call_at(
                    &worker_id,
                    revision,
                    &grant,
                    ToolCall::new("write_file", json!({"path":"never","contents":"never"})),
                )
                .map_err(AgentError)
        })
        .await
    });
    started_rx.await.unwrap();
    response.abort();
    let _ = response.await;
    begin_shutdown(&app);
    assert!(app.agent.begin(None).is_err());
    let shutdown_app = app.clone();
    let closing = tokio::spawn(async move { shutdown(&shutdown_app).await });
    tokio::task::yield_now().await;
    assert!(!closing.is_finished());
    // Wait for durable cancellation, not for a timing-based guess.
    for _ in 0..100 {
        if matches!(
            AgentStore::new(&app).unwrap().load(&id).unwrap().state,
            TaskState::Cancelled { .. }
        ) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        AgentStore::new(&app).unwrap().load(&id).unwrap().state,
        TaskState::Cancelled { .. }
    ));
    {
        let (lock, wake) = &*barrier;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    closing.await.unwrap().unwrap();
    app.tasks.drain().await;
    assert!(!f.roots[0].join("never").exists());
    assert!(app.agent.work.inner.lock().unwrap().jobs.is_empty());
}

async fn selected_model_fixture(
    f: &Fixture,
    replies: Vec<Value>,
) -> (
    Value,
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();
    let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
    let server=axum::Router::new().route("/v1/models",get(||async{Json(json!({"data":[{"id":"synthetic-task-model"}]}))})).route("/v1/chat/completions",post(move|Json(request):Json<Value>|{let replies=replies.clone();let counted=counted.clone();async move{
        assert_eq!(request["model"],"synthetic-task-model");counted.fetch_add(1,Ordering::SeqCst);
        let reply=replies.lock().unwrap().pop_front().expect("unexpected extra model call");
        Json(json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":reply.to_string()}}]}))
    }}));
    let handle = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    let app = f.app();
    let config = crate::Config {
        provider: "openai".into(),
        endpoint: format!("http://{address}/v1"),
        model: "synthetic-task-model".into(),
    };
    let _ = crate::set_config(State(app.clone()), Json(config.clone()))
        .await
        .unwrap();
    let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":config.provider,"endpoint":config.endpoint});
    let _ = crate::test_connection(
        State(app.clone()),
        Json(serde_json::from_value(expected.clone()).unwrap()),
    )
    .await
    .unwrap();
    (expected, count, handle)
}

#[tokio::test]
async fn guided_model_uses_selected_fixture_reads_then_stops_at_exact_write_approval() {
    let mut f = Fixture::new();
    let(expected,count,server)=selected_model_fixture(&f,vec![json!({"content":"Inspect the granted workspace","tool_call":{"name":"list_directory","arguments":{}}}),json!({"content":"Save the requested file","tool_call":{"name":"write_file","arguments":{"path":"model.txt","contents":SENTINEL}}}),json!({"content":"Saved and checked.","tool_call":null})]).await;
    let selected = f.post("/api/agent/model-selection", expected.clone()).await;
    let (id, task) = f
        .setup_scoped(
            json!(["model_inference", "list", "write"]),
            json!([{"kind":"provider_binding","id":selected["scope_id"]}]),
        )
        .await;
    let pending=f.post(&format!("/api/agent/tasks/{id}/model-run"),json!({"expected_revision":task["task"]["revision"],"expected":expected,"max_steps":8,"confirmed":true})).await;
    assert_eq!(pending["task"]["state"]["kind"], "needs_approval");
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(!f.roots[0].join("model.txt").exists());
    let saved = f.approve(&id, &pending).await;
    assert_eq!(
        std::fs::read_to_string(f.roots[0].join("model.txt")).unwrap(),
        SENTINEL
    );
    let done=f.post(&format!("/api/agent/tasks/{id}/model-run"),json!({"expected_revision":saved["task"]["revision"],"expected":expected,"max_steps":1,"confirmed":true})).await;
    assert_eq!(done["task"]["state"]["kind"], "completed");
    assert_eq!(done["task"]["model_steps"], 3);
    assert_eq!(count.load(Ordering::SeqCst), 3);
    let trace = f.get(&format!("/api/agent/tasks/{id}/trace")).await;
    assert!(trace.to_string().contains("model_started"));
    assert!(trace.to_string().contains("model_finished"));
    f.assert_encrypted();
    server.abort();
}

#[tokio::test]
async fn model_refuses_ungranted_provider_and_tool_injection_without_dispatch() {
    let mut f = Fixture::new();
    let(expected,count,server)=selected_model_fixture(&f,vec![json!({"content":"Escalate","tool_call":{"name":"write_file","arguments":{"path":"injected","contents":"bad"}}})]).await;
    let selected = f.post("/api/agent/model-selection", expected.clone()).await;
    let (id, task) = f
        .setup_scoped(json!(["model_inference", "list"]), json!([]))
        .await;
    let input = json!({"expected_revision":task["task"]["revision"],"expected":expected,"max_steps":1,"confirmed":true});
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-run"),
            input.clone()
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (id, task) = f
        .setup_scoped(
            json!(["model_inference", "list"]),
            json!([{"kind":"provider_binding","id":selected["scope_id"]}]),
        )
        .await;
    let mut input = input;
    input["expected_revision"] = task["task"]["revision"].clone();
    assert_eq!(
        f.call(
            Method::POST,
            &format!("/api/agent/tasks/{id}/model-run"),
            input
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(!f.roots[1].join("injected").exists());
    let task = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(task["task"]["state"]["kind"], "running");
    assert_eq!(task["task"]["model_steps"], 1);
    server.abort();
}

#[tokio::test]
async fn platform_fixture_requires_exact_plan_and_user_approval_and_expires_at_restart() {
    let mut f = Fixture::new();
    let (backend, control) = smith_platform::fixture::FixtureBackend::new();
    let app = f.app();
    let store = AgentStore::new(&app).unwrap();
    let mut session =
        smith_platform::PlatformSession::new(store.owner().id(), Box::new(backend)).unwrap();
    session
        .select_target(
            smith_platform::TargetRequest::SyntheticFixture,
            smith_platform::TargetPermissions {
                read_elements: true,
                interact: true,
            },
        )
        .unwrap();
    let binding = session.binding().to_owned();
    *app.agent.platform.lock().unwrap() = Some(Arc::new(platform::Platform::from_session(session)));
    let (id, task) = f
        .setup_scoped(
            json!(["computer", "system_monitor"]),
            json!([{"kind":"effect_binding","id":binding}]),
        )
        .await;
    let snapshot = f
        .post(
            "/api/agent/platform/snapshot",
            json!({"confirmed":true,"focus_return_delay_ms":0}),
        )
        .await;
    let plan=f.post("/api/agent/platform/prepare",json!({"snapshot_id":snapshot["snapshot"]["id"],"action":{"kind":"click","x":30,"y":30}})).await;
    let mut tampered = plan["call"]["arguments"].clone();
    tampered["review"]["target"]["label"] = json!("Other target");
    assert_eq!(f.call(Method::POST,&format!("/api/agent/tasks/{id}/actions"),json!({"expected_revision":task["task"]["revision"],"request_id":crate::id(),"action":{"kind":"tool","name":"screen_click","arguments":tampered}})).await.0,StatusCode::FORBIDDEN);
    let task = f.get(&format!("/api/agent/tasks/{id}")).await;
    let pending = tool(
        &f,
        &id,
        &task,
        "screen_click",
        plan["call"]["arguments"].clone(),
    )
    .await;
    assert_eq!(control.actions().len(), 0);
    let saved=f.post("/api/agent/platform/approve",json!({"task_id":id,"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"plan_id":plan["plan"]["id"],"confirmed":true,"focus_return_delay_ms":0})).await;
    assert_eq!(control.actions().len(), 1);
    assert_eq!(saved["task"]["state"]["kind"], "running");
    assert_eq!(f.call(Method::POST,"/api/agent/platform/approve",json!({"task_id":id,"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"plan_id":plan["plan"]["id"],"confirmed":true,"focus_return_delay_ms":0})).await.0,StatusCode::CONFLICT);
    drop(app);
    drop(store);
    f.restart().await;
    assert_eq!(
        f.call(Method::GET, "/api/agent/platform/session", Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(control.actions().len(), 1);
}

#[tokio::test]
async fn effect_fixture_requires_retained_exact_plan_and_cannot_replay_approval() {
    let mut f = Fixture::new();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/fixture");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/fixture",
                post(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        "delivered"
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let binding = f
        .app()
        .agent
        .effects
        .fixture_destination(
            &url,
            address,
            smith_tool_effects::NetworkOperation::SendWebhook,
        )
        .unwrap();
    let (id, task) = f
        .setup_scoped(
            json!(["send_webhook"]),
            json!([{"kind":"effect_binding","id":binding["id"]}]),
        )
        .await;
    let plan=f.post("/api/agent/effects/prepare",json!({"binding_id":binding["id"],"revision":binding["revision"],"tool":"send_webhook","arguments":{"url":url,"message":SENTINEL}})).await;
    let pending = tool(
        &f,
        &id,
        &task,
        "send_webhook",
        plan["call"]["arguments"].clone(),
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let approval = json!({"task_id":id,"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"plan_id":plan["plan"]["id"],"confirmed":true});
    let saved = f.post("/api/agent/effects/approve", approval.clone()).await;
    assert_eq!(saved["task"]["state"]["kind"], "running");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.call(Method::POST, "/api/agent/effects/approve", approval)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    f.restart().await;
    assert_eq!(
        f.get(&format!("/api/agent/tasks/{id}")).await["task"]["state"]["kind"],
        "running"
    );
    server.abort();
}

#[tokio::test]
async fn cancelling_pending_model_step_drains_worker_and_late_reply_cannot_write() {
    let mut f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(Notify::new());
    let observed = entered.clone();
    let gate = release.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/v1/models",get(||async{Json(json!({"data":[{"id":"synthetic-cancellation"}]}))})).route("/v1/chat/completions",post(move||{let observed=observed.clone();let gate=gate.clone();async move{observed.store(true,Ordering::SeqCst);gate.notified().await;Json(json!({"choices":[{"finish_reason":"stop","message":{"content":json!({"content":"Late proposal","tool_call":{"name":"write_file","arguments":{"path":"too-late","contents":SENTINEL}}}).to_string()}}]}))}}))).await.unwrap();
    });
    let app = f.app();
    let config = crate::Config {
        provider: "openai".into(),
        endpoint: format!("http://{address}/v1"),
        model: "synthetic-cancellation".into(),
    };
    let _ = crate::set_config(State(app.clone()), Json(config.clone()))
        .await
        .unwrap();
    let expected = json!({"revision":app.consent.lock().unwrap().token(),"provider":config.provider,"endpoint":config.endpoint});
    let _ = crate::test_connection(
        State(app.clone()),
        Json(serde_json::from_value(expected.clone()).unwrap()),
    )
    .await
    .unwrap();
    let selected = f.post("/api/agent/model-selection", expected.clone()).await;
    let (id, task) = f
        .setup_scoped(
            json!(["model_inference", "write"]),
            json!([{"kind":"provider_binding","id":selected["scope_id"]}]),
        )
        .await;
    let path = format!("/api/agent/tasks/{id}/model-run");
    let request_app = app.clone();
    let worker = tokio::spawn(async move {
        request(request_app,Method::POST,&path,json!({"expected_revision":task["task"]["revision"],"expected":expected,"max_steps":8,"confirmed":true})).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let thinking = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(thinking["task"]["state"]["kind"], "model_thinking");
    let cancelled=f.post(&format!("/api/agent/tasks/{id}/cancel"),json!({"expected_revision":thinking["task"]["revision"],"reason":"Explicit cancellation"})).await;
    assert_eq!(cancelled["task"]["state"]["kind"], "cancelled");
    let response = tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(!response.0.is_success());
    release.notify_waiters();
    app.tasks.drain().await;
    assert!(app.agent.work.inner.lock().unwrap().jobs.is_empty());
    assert!(!f.roots[0].join("too-late").exists());
    let final_task = f.get(&format!("/api/agent/tasks/{id}")).await;
    assert_eq!(
        final_task["task"]["revision"],
        cancelled["task"]["revision"]
    );
    assert!(final_task["task"]["last_model_reply"].is_null());
    server.abort();
}

#[tokio::test]
async fn grants_freeze_at_task_creation_and_host_audit_recovery_never_replays() {
    let mut f = Fixture::new();
    let (id, task) = f.setup(json!(["list"])).await;
    let workspace = task["workspace"]["id"].as_str().unwrap();
    assert_eq!(f.call(Method::POST,&format!("/api/agent/workspaces/{workspace}/grants"),json!({"expected_policy_revision":1,"capabilities":["list","write"],"resources":[],"confirmed":true})).await.0,StatusCode::CONFLICT);
    let store = AgentStore::new(&f.app()).unwrap();
    let audit = store
        .begin_host_operation("synthetic_unstarted_effect", &json!({"sentinel":SENTINEL}))
        .unwrap();
    drop(store);
    f.restart().await;
    let operations = f.get("/api/agent/effects/audit").await;
    let row = operations["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == audit)
        .unwrap();
    assert_eq!(row["state"], "uncertain");
    let store = AgentStore::new(&f.app()).unwrap();
    store.with_db(|db|{db.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1023) INSERT INTO agent_host_operations(id,principal_id,operation,request_json,state,created_at,updated_at) SELECT 'budget-'||i,?1,'fixture','{}','finished','1','1' FROM n",[store.owner().id()]).unwrap();Ok(())}).unwrap();
    assert!(matches!(
        store.begin_host_operation("git_init", &json!({})),
        Err(Error::LimitExceeded(_))
    ));

    assert_eq!(
        f.get(&format!("/api/agent/tasks/{id}")).await["task"]["revision"],
        task["task"]["revision"]
    );
    f.assert_encrypted();
}

// Covers the real catalog/immutable-executable/Seatbelt path reached by the
// browser. Network fixtures cannot detect executable publication failures.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires SMITH_TEST_CATALOG_BINARY pointing to the built normal Smith executable"]
async fn approved_catalog_echo_runs_once_and_reports_reaped_output() {
    assert!(
        std::env::var_os("SMITH_TEST_CATALOG_BINARY").is_some(),
        "Use a frozen normal Smith executable; the libtest executable has a different entrypoint"
    );
    let mut f = Fixture::new();
    let created = f.post("/api/agent/workspaces", json!({"label":"Catalog fixture workspace","capabilities":["shell"],"resources":[],"confirmed":true})).await;
    let workspace = created["workspace"]["id"].as_str().unwrap();
    let store = AgentStore::new(&f.app()).unwrap();
    let policy = store.policy(workspace).unwrap();
    f.roots.push(
        Workspace::reopen_temporary(&policy.identity)
            .unwrap()
            .host_path()
            .to_path_buf(),
    );
    let binding = f
        .post(
            "/api/agent/effects/bind-workspace",
            json!({"workspace_id":workspace,"executables":["echo"],"git":false,"confirmed":true}),
        )
        .await;
    let granted = f.post(&format!("/api/agent/workspaces/{workspace}/grants"), json!({"expected_policy_revision":1,"capabilities":["shell"],"resources":[binding["resource"]],"confirmed":true})).await;
    let task = f.post("/api/agent/tasks", json!({"workspace_id":workspace,"policy_revision":granted["workspace"]["policy_revision"],"input":SENTINEL})).await;
    let id = task["task"]["id"].as_str().unwrap();
    let task = f
        .post(
            &format!("/api/agent/tasks/{id}/start"),
            json!({"expected_revision":0}),
        )
        .await;
    let plan = f.post("/api/agent/effects/prepare", json!({"binding_id":binding["binding"]["id"],"revision":binding["binding"]["revision"],"tool":"run_shell","arguments":{"command":format!("echo {SENTINEL}")}})).await;
    let pending = tool(
        &f,
        id,
        &task,
        "run_shell",
        plan["call"]["arguments"].clone(),
    )
    .await;
    assert_eq!(pending["task"]["state"]["kind"], "needs_approval");
    assert!(pending["task"]["last_result"].is_null());
    let approval = json!({"task_id":id,"expected_revision":pending["task"]["revision"],"call_id":pending["task"]["state"]["call"]["id"],"plan_id":plan["plan"]["id"],"confirmed":true});
    let saved = f.post("/api/agent/effects/approve", approval.clone()).await;
    let result = &saved["task"]["last_result"];
    assert_eq!(result["exit_code"], 0, "{saved}");
    assert_eq!(result["stdout"], format!("{SENTINEL}\n"));
    assert_eq!(result["reaped"], true);
    assert_eq!(result["sandbox"], "macos-seatbelt");
    assert_eq!(
        f.call(Method::POST, "/api/agent/effects/approve", approval)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.get(&format!("/api/agent/tasks/{id}")).await["task"]["revision"],
        saved["task"]["revision"]
    );
    f.assert_encrypted();
}
