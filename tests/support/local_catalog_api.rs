//! Synthetic selected files and a real keyed profile; no runtime is executed.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request},
};
use smith_profile_storage as storage;
use std::os::unix::fs::PermissionsExt;
use tower::ServiceExt;

struct Fixture {
    profile: tempfile::TempDir,
    files: tempfile::TempDir,
    app: Option<Arc<App>>,
}
impl Fixture {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let path = profile
            .path()
            .canonicalize()
            .unwrap()
            .join(storage::DB_FILE);
        let db = storage::create(
            &path,
            &storage::Secret::new("synthetic catalog passphrase".into()).unwrap(),
        )
        .unwrap();
        let files = tempfile::tempdir().unwrap();
        std::fs::write(
            files.path().join("runner"),
            b"\xcf\xfa\xed\xfeSYNTHETIC HEADER ONLY; NEVER RUN",
        )
        .unwrap();
        std::fs::set_permissions(
            files.path().join("runner"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let mut gguf = b"GGUF".to_vec();
        gguf.extend_from_slice(&3u32.to_le_bytes());
        gguf.extend_from_slice(&0u64.to_le_bytes());
        gguf.extend_from_slice(&0u64.to_le_bytes());
        gguf.resize(64, 0);
        std::fs::write(files.path().join("model.gguf"), gguf).unwrap();
        Self {
            profile,
            files,
            app: Some(app_from_connection(db).unwrap()),
        }
    }
    fn app(&self) -> Arc<App> {
        self.app.as_ref().unwrap().clone()
    }
    fn selection(&self) -> Value {
        let root = self.files.path().canonicalize().unwrap();
        let source = json!({"label":"synthetic selected files","revision":"v1","origin":"external_user_selected"});
        json!({"description":{"format":"gguf","architecture":"synthetic","declared_capabilities":["text_chat"],"declared_context_limit":4096,"hints":{"adapter":"llama_gguf","hints":{"threads":7}}},
          "runtime":{"root":root,"source":source,"files":[{"name":"runner","role":"executable"}]},
          "model":{"root":root,"source":source,"files":[{"name":"model.gguf","role":"weights"}]}})
    }
    fn expected(&self) -> Value {
        let a = self.app();
        let consent = a.consent.lock().unwrap();
        let config = a.config.lock().unwrap();
        json!({"revision":consent.token(),"provider":config.provider,"endpoint":config.endpoint})
    }
    async fn call(&self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
        let response = routes()
            .merge(acceleration_api::routes())
            .merge(mlx_api::routes())
            .with_state(self.app())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(if body.is_null() {
                        Body::empty()
                    } else {
                        Body::from(body.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 3 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
        )
    }
    async fn ok(&self, method: Method, path: &str, body: Value) -> Value {
        let (status, value) = self.call(method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        value
    }
    async fn list(&self) -> Value {
        self.ok(Method::GET, "/api/local-catalog", Value::Null)
            .await
    }
    async fn wait(&self, response: &Value) -> Value {
        self.app().tasks.drain().await;
        self.ok(
            Method::GET,
            &format!(
                "/api/local-catalog/operations/{}",
                response["operation_id"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await["operation"]
            .clone()
    }
    async fn register(&self) -> (Value, Value) {
        let input = json!({"request_id":id(),"catalog":self.list().await["catalog"],"label":"PRIVATE_CATALOG_LABEL_7429","selection":self.selection()});
        let (status, response) = self
            .call(Method::POST, "/api/local-catalog/entries", input.clone())
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{response}");
        let op = self.wait(&response).await;
        assert_eq!(op["status"], "succeeded", "{op}");
        (input, op["result"].clone())
    }
    async fn restart(&mut self) {
        let a = self.app.take().unwrap();
        a.local_catalog.begin_shutdown();
        a.tasks.drain().await;
        a.local_catalog.shutdown_result().unwrap();
        a.native.shutdown().await.unwrap();
        a.mlx.shutdown().await.unwrap();
        let owned = Arc::try_unwrap(a).ok().expect("no detached catalog owners");
        storage::close(owned.db.into_inner().unwrap()).unwrap();
        let path = self
            .profile
            .path()
            .canonicalize()
            .unwrap()
            .join(storage::DB_FILE);
        let db = storage::unlock(
            &path,
            &storage::Secret::new("synthetic catalog passphrase".into()).unwrap(),
        )
        .unwrap();
        self.app = Some(app_from_connection(db).unwrap());
    }
    async fn select_prepare(&self, entry: &Value) -> Value {
        let list = self.list().await;
        self.ok(Method::POST,"/api/local-catalog/select",json!({"request_id":id(),"catalog":list["catalog"],"entry_id":entry["id"],"entry_revision":entry["revision"]})).await;
        self.inventory("synthetic-generation");
        let list = self.list().await;
        let (status,response)=self.call(Method::POST,"/api/local-catalog/prepare",json!({"catalog":list["catalog"],"entry_id":entry["id"],"entry_revision":entry["revision"],"expected":self.expected()})).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{response}");
        let op = self.wait(&response).await;
        assert_eq!(op["status"], "succeeded", "{op}");
        op["result"]["prepared"].clone()
    }
    fn inventory(&self, generation: &str) {
        use smith_acceleration::*;
        acceleration_api::set_catalog_inventory_for_test(
            &self.app(),
            hardware_inventory::Observation {
                platform_evidence: None,
                schema_version: 1,
                source: "synthetic_no_os_query",
                observed_unix_ms: now() * 1000,
                inventory: HardwareInventory {
                    generation: generation.into(),
                    os: OperatingSystem::Macos,
                    architecture: Architecture::Aarch64,
                    os_version: "synthetic".into(),
                    logical_cpu_count: 1,
                    host_memory_pool_id: "host".into(),
                    memory_pools: vec![MemoryPool {
                        id: "host".into(),
                        total_bytes: Some(1 << 30),
                        available_bytes: None,
                    }],
                    devices: vec![Device {
                        id: "cpu".into(),
                        class: DeviceClass::Cpu,
                        vendor: Vendor::Apple,
                        memory_pool_id: "host".into(),
                        driver_version: None,
                    }],
                },
                process_architecture: "aarch64",
                physical_cpu_count: Some(1),
                active_cpu_count: Some(1),
                identity_scope: hardware_inventory::IdentityScope::ProcessLifetime,
                metal_devices: vec![],
                memory_sample: None,
                limitations: vec!["synthetic"],
                backend_runtime_tested: false,
                model_placement_observed: false,
            },
        );
    }
}

#[tokio::test]
async fn absent_malformed_and_wrong_storage_types_are_distinct_without_reset() {
    let f = Fixture::new();
    assert_eq!(
        f.list().await["catalog"],
        json!({"revision":0,"sha256":null})
    );
    for invalid in ["null", "{}", "{\"format_version\":99}"] {
        f.app()
            .db
            .lock()
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
                params![SETTINGS_KEY, invalid],
            )
            .unwrap();
        assert_eq!(
            f.call(Method::GET, "/api/local-catalog", Value::Null)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let raw: String = f
            .app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [SETTINGS_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, invalid);
    }
    f.app()
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE settings SET value=x'7b7d' WHERE key=?1",
            [SETTINGS_KEY],
        )
        .unwrap();
    assert_eq!(
        f.call(Method::GET, "/api/local-catalog", Value::Null)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(serde_json::from_value::<CatalogVersion>(json!({"revision":0})).is_err());
}

#[tokio::test]
async fn lost_capture_reply_replays_after_restart_and_source_deletion_without_duplicate() {
    let mut f = Fixture::new();
    let (input, result) = f.register().await;
    let before = f.list().await;
    std::fs::remove_file(f.files.path().join("model.gguf")).unwrap();
    std::fs::remove_file(f.files.path().join("runner")).unwrap();
    f.restart().await;
    assert_eq!(f.list().await["entries"], before["entries"]);
    let replay = f
        .ok(Method::POST, "/api/local-catalog/entries", input.clone())
        .await;
    assert_eq!(replay["receipt"], result["receipt"]);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["catalog"], before["catalog"]);
    let mut changed = input.clone();
    changed["label"] = json!("different operation");
    assert_eq!(
        f.call(Method::POST, "/api/local-catalog/entries", changed)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let recovered = f
        .ok(
            Method::GET,
            &format!(
                "/api/local-catalog/requests/{}",
                input["request_id"].as_str().unwrap()
            ),
            Value::Null,
        )
        .await;
    assert_eq!(recovered["receipt"]["result"], result["receipt"]);
    let bytes = std::fs::read(f.profile.path().join(storage::DB_FILE)).unwrap();
    assert!(!bytes
        .windows(b"PRIVATE_CATALOG_LABEL_7429".len())
        .any(|w| w == b"PRIVATE_CATALOG_LABEL_7429"));
}

#[tokio::test]
async fn trash_restore_and_rename_replay_preserve_files_and_do_not_apply_hints() {
    let f = Fixture::new();
    let (_, r) = f.register().await;
    let entry = &r["receipt"]["entry"];
    let a = f.app();
    let preferences = runtime_preferences::read(&a.db.lock().unwrap()).unwrap();
    let config = serde_json::to_value(&*a.config.lock().unwrap()).unwrap();
    let selected=f.ok(Method::POST,"/api/local-catalog/select",json!({"request_id":id(),"catalog":r["catalog"],"entry_id":entry["id"],"entry_revision":entry["revision"]})).await;
    let trash_input =
        json!({"request_id":id(),"catalog":selected["catalog"],"entry_revision":entry["revision"]});
    let path = format!(
        "/api/local-catalog/entries/{}",
        entry["id"].as_str().unwrap()
    );
    let trashed = f.ok(Method::DELETE, &path, trash_input.clone()).await;
    assert!(f.list().await["entries"].as_array().unwrap().is_empty());
    assert!(f.list().await["selected_for_review"].is_null());
    let replay = f.ok(Method::DELETE, &path, trash_input).await;
    assert_eq!(replay["receipt"], trashed["receipt"]);
    let restored=f.ok(Method::POST,&format!("{path}/restore"),json!({"request_id":id(),"catalog":trashed["catalog"],"entry_revision":trashed["receipt"]["entry"]["revision"]})).await;
    assert_eq!(restored["receipt"]["entry"]["selected_for_review"], false);
    std::fs::remove_file(f.files.path().join("model.gguf")).unwrap();
    let renamed=f.ok(Method::POST,&format!("{path}/rename"),json!({"request_id":id(),"catalog":restored["catalog"],"entry_revision":restored["receipt"]["entry"]["revision"],"label":"Renamed without touching missing source"})).await;
    assert_eq!(
        renamed["receipt"]["entry"]["label"],
        "Renamed without touching missing source"
    );
    assert_eq!(
        serde_json::to_value(&*a.config.lock().unwrap()).unwrap(),
        config
    );
    assert_eq!(
        runtime_preferences::read(&a.db.lock().unwrap()).unwrap(),
        preferences
    );
    assert_eq!(a.native.revision(), 0);
    assert!(!a.mlx.occupied());
}

#[tokio::test]
async fn exact_raw_fingerprint_rejects_same_revision_reencoding_and_failed_sql_is_atomic() {
    let f = Fixture::new();
    let (_, r) = f.register().await;
    let entry = &r["receipt"]["entry"];
    let path = format!(
        "/api/local-catalog/entries/{}/rename",
        entry["id"].as_str().unwrap()
    );
    let request = json!({"request_id":id(),"catalog":r["catalog"],"entry_revision":entry["revision"],"label":"new"});
    f.app()
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE settings SET value=value||' ' WHERE key=?1",
            [SETTINGS_KEY],
        )
        .unwrap();
    assert_eq!(
        f.call(Method::POST, &path, request.clone()).await.0,
        StatusCode::CONFLICT
    );
    let before = f.list().await;
    let mut current = request;
    current["catalog"] = before["catalog"].clone();
    f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER refuse_catalog BEFORE UPDATE ON settings WHEN NEW.key='local_model_catalog' BEGIN SELECT RAISE(ABORT,'synthetic publication failure'); END;").unwrap();
    assert!(f
        .call(Method::POST, &path, current.clone())
        .await
        .0
        .is_server_error());
    assert_eq!(f.list().await, before);
    assert_eq!(
        f.call(
            Method::GET,
            &format!(
                "/api/local-catalog/requests/{}",
                current["request_id"].as_str().unwrap()
            ),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    f.app()
        .db
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_catalog")
        .unwrap();
    f.ok(Method::POST, &path, current).await;
}

#[tokio::test]
async fn capture_replacement_is_explicit_and_concurrent_exact_request_is_once_only() {
    let f = Fixture::new();
    let input = json!({"request_id":id(),"catalog":f.list().await["catalog"],"label":"same","selection":f.selection()});
    let (a, b) = tokio::join!(
        f.call(Method::POST, "/api/local-catalog/entries", input.clone()),
        f.call(Method::POST, "/api/local-catalog/entries", input)
    );
    assert!([StatusCode::OK, StatusCode::ACCEPTED].contains(&a.0));
    assert!([StatusCode::OK, StatusCode::ACCEPTED].contains(&b.0));
    f.app().tasks.drain().await;
    let list = f.list().await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);
    assert_eq!(list["catalog"]["revision"], 1);
    let entry = &list["entries"][0];
    std::fs::write(f.files.path().join("model.gguf"), b"changed selected bytes").unwrap();
    let replacement = json!({"request_id":id(),"catalog":list["catalog"],"entry_revision":entry["revision"],"label":"explicit replacement","selection":f.selection()});
    let (s, v) = f
        .call(
            Method::PUT,
            &format!(
                "/api/local-catalog/entries/{}",
                entry["id"].as_str().unwrap()
            ),
            replacement,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let op = f.wait(&v).await;
    assert_eq!(op["status"], "succeeded");
    assert_eq!(op["result"]["receipt"]["entry"]["id"], entry["id"]);
    assert_eq!(op["result"]["catalog"]["revision"], 2);
}

#[tokio::test]
async fn cancellation_and_shutdown_wait_for_owned_worker_then_prevent_publication() {
    let f = Fixture::new();
    let a = f.app();
    let version = read(&a.db.lock().unwrap()).unwrap().version();
    let (lease, control, response) = a
        .local_catalog
        .begin("synthetic_blocked_read", version, None, None)
        .unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let published = finished.clone();
    a.spawn_task(async move {
        release_rx.await.unwrap();
        assert!(control.is_cancelled());
        lease.publish(|| {
            published.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({"wrong":true}))
        });
    });
    let cancel_response = f
        .ok(
            Method::POST,
            &format!(
                "/api/local-catalog/operations/{}/cancel",
                response["operation_id"].as_str().unwrap()
            ),
            json!({}),
        )
        .await;
    assert_eq!(cancel_response["operation"]["worker_drained"], false);
    a.local_catalog.begin_shutdown();
    assert!(a.local_catalog.shutdown_result().is_err());
    release_tx.send(()).unwrap();
    a.tasks.drain().await;
    a.local_catalog.shutdown_result().unwrap();
    let op = f.wait(&response).await;
    assert_eq!(op["status"], "cancelled");
    assert_eq!(op["worker_drained"], true);
    assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
    assert!(f.list().await["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn prepared_binding_rejects_catalog_connection_hardware_and_session_changes() {
    let mut f = Fixture::new();
    let (_, r) = f.register().await;
    let entry = &r["receipt"]["entry"];
    let review = f.select_prepare(entry).await;
    let a = f.app();
    let prepared = {
        let consent = a.consent.lock().unwrap();
        prepared_locked(&a, &consent, review["id"].as_str().unwrap()).unwrap()
    };
    f.inventory("changed-generation");
    {
        let c = a.consent.lock().unwrap();
        assert!(validate_binding_locked(&a, &c, &prepared.binding()).is_err());
    }
    f.inventory("synthetic-generation");
    {
        let mut c = a.consent.lock().unwrap();
        c.advance();
        assert!(validate_binding_locked(&a, &c, &prepared.binding()).is_err());
    }
    drop(a);
    f.restart().await;
    f.inventory("synthetic-generation");
    let a = f.app();
    let c = a.consent.lock().unwrap();
    assert!(validate_binding_locked(&a, &c, &prepared.binding()).is_err());
}

#[tokio::test]
async fn gguf_handoff_only_inspects_and_requires_exact_bytes_without_applying_declared_hints() {
    let f = Fixture::new();
    let (_, r) = f.register().await;
    let review = f.select_prepare(&r["receipt"]["entry"]).await;
    let a = f.app();
    let before = serde_json::to_value(&*a.config.lock().unwrap()).unwrap();
    let result=f.ok(Method::POST,"/api/local-catalog/setup",json!({"adapter":"gguf","review_id":review["id"],"model_id":"synthetic-catalog-model","settings_revision":0,"expected":f.expected()})).await;
    assert!(result["operation_id"].is_string());
    a.tasks.drain().await;
    let status = f.ok(Method::GET, "/api/acceleration", Value::Null).await;
    assert_eq!(status["operation"]["status"], "succeeded", "{status}");
    assert_eq!(status["selection"]["catalog"]["id"], review["id"]);
    assert!(status["selection"]["probe"].is_object());
    assert!(status["prepared"].is_null());
    assert_eq!(
        serde_json::to_value(&*a.config.lock().unwrap()).unwrap(),
        before
    );
    assert_eq!(a.native.revision(), 0);
    assert!(!a.mlx.occupied());
    assert!(runtime_preferences::read(&a.db.lock().unwrap())
        .unwrap()
        .intent
        .overrides
        .threads
        .is_none());
    std::fs::write(
        f.files.path().join("runner"),
        b"\xcf\xfa\xed\xfechanged synthetic header; never run",
    )
    .unwrap();
    f.ok(Method::POST,"/api/local-catalog/setup",json!({"adapter":"gguf","review_id":review["id"],"model_id":"synthetic-catalog-model","settings_revision":0,"expected":f.expected()})).await;
    a.tasks.drain().await;
    let status = f.ok(Method::GET, "/api/acceleration", Value::Null).await;
    assert_eq!(status["operation"]["status"], "failed");
    assert!(status["selection"].is_null());
    assert_eq!(a.native.revision(), 0);
}

#[tokio::test]
async fn same_byte_symlink_substitution_is_not_the_reviewed_catalog_path() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let (_, r) = f.register().await;
    let review = f.select_prepare(&r["receipt"]["entry"]).await;
    let original = f.files.path().join("model.gguf");
    let moved = f.files.path().join("different-model.gguf");
    std::fs::rename(&original, &moved).unwrap();
    symlink(&moved, &original).unwrap();
    f.ok(Method::POST,"/api/local-catalog/setup",json!({"adapter":"gguf","review_id":review["id"],"model_id":"synthetic-alias","settings_revision":0,"expected":f.expected()})).await;
    f.app().tasks.drain().await;
    let status = f.ok(Method::GET, "/api/acceleration", Value::Null).await;
    assert_eq!(status["operation"]["status"], "failed", "{status}");
    assert!(status["selection"].is_null());
    assert_eq!(f.app().native.revision(), 0);
}

#[tokio::test]
async fn mlx_normalized_resource_alias_cannot_hide_a_reviewed_file() {
    let f = Fixture::new();
    let root = f.files.path().canonicalize().unwrap();
    std::fs::write(
        root.join("smith-mlx-worker"),
        std::fs::read(root.join("runner")).unwrap(),
    )
    .unwrap();
    for name in [
        "config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "weights.safetensors",
    ] {
        std::fs::write(root.join(name), b"synthetic never loaded").unwrap();
    }
    let mut selection = f.selection();
    selection["description"] = json!({"format":"mlx","architecture":"synthetic","declared_capabilities":["raw_completion"],"declared_context_limit":512,"hints":{"adapter":"mlx_swift","hints":{}}});
    selection["runtime"]["files"] = json!([{"name":"runner","role":"executable"},{"name":"smith-mlx-worker","role":"runtime_resource"}]);
    selection["model"]["files"] = json!([{"name":"config.json","role":"model_metadata"},{"name":"tokenizer.json","role":"model_metadata"},{"name":"tokenizer_config.json","role":"model_metadata"},{"name":"weights.safetensors","role":"weights"}]);
    let (s,v)=f.call(Method::POST,"/api/local-catalog/entries",json!({"request_id":id(),"catalog":f.list().await["catalog"],"label":"MLX alias fixture","selection":selection})).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let op = f.wait(&v).await;
    assert_eq!(op["status"], "succeeded", "{op}");
    let review = f.select_prepare(&op["result"]["receipt"]["entry"]).await;
    let a = f.app();
    let selected = {
        let c = a.consent.lock().unwrap();
        prepared_locked(&a, &c, review["id"].as_str().unwrap()).unwrap()
    };
    let m = &selected.entry().manifest;
    let worker = &m
        .runtime
        .files
        .iter()
        .find(|f| f.role == catalog::FileRole::Executable)
        .unwrap();
    let plan = smith_mlx_runtime::PlanSummary {
        id: "synthetic-never-executed".into(),
        worker: smith_managed_runtime::Artifact {
            path: root.join("runner"),
            sha256: worker.sha256.clone(),
            source: "synthetic".into(),
            source_revision: "v1".into(),
        },
        runtime_files: vec![smith_mlx_runtime::FileRecord {
            name: "smith-mlx-worker".into(),
            bytes: worker.bytes,
            sha256: worker.sha256.clone(),
        }],
        runtime_bundle_sha256: "0".repeat(64),
        model: smith_mlx_runtime::ModelSelection {
            root: root.clone(),
            files: m
                .model
                .files
                .iter()
                .map(|f| smith_mlx_runtime::FileRecord {
                    name: f.name.clone(),
                    bytes: f.bytes,
                    sha256: f.sha256.clone(),
                })
                .collect(),
            package_sha256: "1".repeat(64),
        },
        budgets: smith_mlx_runtime::Budgets::default(),
        backend: "synthetic",
        execution_kind: smith_mlx_runtime::ExecutionKind::Native,
        execution_notice: "comparison only",
    };
    assert!(selected.require_mlx(&plan).is_err());
    assert!(!a.mlx.occupied());
}

#[test]
fn abandoned_catalog_worker_never_claims_drain_or_admits_replacement() {
    let state = CatalogState::default();
    let version = CatalogVersion {
        revision: 0,
        sha256: None,
    };
    let (lease, control, _) = state
        .begin("synthetic_abandoned", version.clone(), None, None)
        .unwrap();
    drop(lease);
    assert!(control.is_cancelled());
    assert!(state.shutdown_result().is_err());
    assert!(state.begin("replacement", version, None, None).is_err());
    let inner = state.shared.inner.lock().unwrap();
    assert_eq!(inner.operations[0].status, "failed");
    assert!(!inner.operations[0].worker_drained);
}

#[tokio::test]
async fn recreated_gguf_plan_requires_current_catalog_review_and_manual_plans_require_none() {
    let f = Fixture::new();
    let (_, r) = f.register().await;
    let first = f.select_prepare(&r["receipt"]["entry"]).await;
    let a = f.app();
    let selected = {
        let c = a.consent.lock().unwrap();
        prepared_locked(&a, &c, first["id"].as_str().unwrap()).unwrap()
    };
    let files = runtime_catalog::inspect_selection(
        selected
            .selected_file(true, catalog::FileRole::Executable)
            .unwrap(),
        selected
            .selected_file(false, catalog::FileRole::Weights)
            .unwrap(),
        &smith_managed_runtime::Cancellation::new(),
    )
    .await
    .unwrap();
    let manifest = smith_managed_runtime::Manifest {
        model_id: "synthetic-repeat-plan".into(),
        executable: files.executable,
        model: files.model,
        protocol: smith_managed_runtime::Protocol::LlamaCppUnixHttpV1,
        execution_kind: smith_managed_runtime::ExecutionKind::CompatibleFixture,
        declares_text_chat: true,
    };
    let plan = smith_managed_runtime::LoadPlan::prepare(
        manifest.clone(),
        smith_managed_runtime::LoadConfig::default(),
        &smith_managed_runtime::Cancellation::new(),
    )
    .await
    .unwrap();
    let recreated = smith_managed_runtime::LoadPlan::prepare(
        manifest,
        smith_managed_runtime::LoadConfig::default(),
        &smith_managed_runtime::Cancellation::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        plan.summary().id,
        recreated.summary().id,
        "the regression requires a deterministic repeated plan ID"
    );
    let saved = f.list().await;
    let e = &saved["entries"][0];
    let renamed=f.ok(Method::POST,&format!("/api/local-catalog/entries/{}/rename",e["id"].as_str().unwrap()),json!({"request_id":id(),"catalog":saved["catalog"],"entry_revision":e["revision"],"label":"new review label, exact same files"})).await;
    let second = f.select_prepare(&renamed["receipt"]["entry"]).await;
    assert_ne!(first["id"], second["id"]);
    let current = {
        let c = a.consent.lock().unwrap();
        prepared_locked(&a, &c, second["id"].as_str().unwrap()).unwrap()
    };
    acceleration_api::stage_catalog_apply_for_test(&a, recreated.clone(), Some(current.binding()));
    for review in [Value::Null, first["id"].clone()] {
        let mut request = f.expected();
        request["plan_id"] = json!(plan.summary().id);
        request["expected_revision"] = json!(0);
        request["confirm"] = json!(true);
        request["catalog_review_id"] = review;
        let (status, error) = f
            .call(Method::POST, "/api/acceleration/apply", request)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(error.to_string().contains("catalog review"));
        let status = f.ok(Method::GET, "/api/acceleration", Value::Null).await;
        assert!(status["operation"].is_null());
        assert!(status["prepared"].is_object());
        assert_eq!(a.native.revision(), 0);
    }
    // Manual preparation cannot be reinterpreted as the catalog approval either.
    acceleration_api::stage_catalog_apply_for_test(&a, recreated, None);
    let mut request = f.expected();
    request["plan_id"] = json!(plan.summary().id);
    request["expected_revision"] = json!(0);
    request["confirm"] = json!(true);
    request["catalog_review_id"] = second["id"].clone();
    assert_eq!(
        f.call(Method::POST, "/api/acceleration/apply", request)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // The existing manual-flow tests exercise admission with the absent field.
    assert!(acceleration_api::manual_catalog_review_absent_for_test(
        json!({"plan_id":"synthetic","expected_revision":0,"confirm":true,"revision":"test","provider":"mock","endpoint":"mock://local"})
    ));
}
