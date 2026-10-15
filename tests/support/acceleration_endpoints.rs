//! Root HTTP router regressions over synthetic SQLCipher profiles. No OS
//! inventory, network provider, real profile, runtime probe or model is used.
//! Include this module at the main crate root under cfg(test).
use crate::*;
use axum::{
    body::{to_bytes, Body},
    http::{Method, Request as HttpRequest},
};
use futures_util::FutureExt;
use smith_profile_storage as storage;
use tower::ServiceExt;

const SECRET: &str = "synthetic acceleration endpoint test profile key";
const BASE: &str = "/api/acceleration";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    path: PathBuf,
    app: Option<Arc<App>>,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join(storage::DB_FILE);
        let connection =
            storage::create(&path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        let cipher: String = connection
            .query_row("PRAGMA cipher_version", [], |row| row.get(0))
            .unwrap();
        assert!(cipher.starts_with('4'));
        Self {
            _directory: directory,
            root,
            path,
            app: Some(app_from_connection(connection).unwrap()),
        }
    }
    fn app(&self) -> Arc<App> {
        self.app.as_ref().unwrap().clone()
    }
    fn call(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
        request(self.app(), method, path, input)
    }
    fn ok(&self, method: Method, path: &str, input: Value) -> Value {
        let (status, result) = self.call(method, path, input);
        assert_eq!(status, StatusCode::OK, "{path}: {result}");
        result
    }
    fn get(&self, path: &str) -> Value {
        self.ok(Method::GET, path, Value::Null)
    }
    fn expected(&self) -> Value {
        let config = self.get("/api/config");
        json!({"revision":config["revision"],"provider":config["provider"],"endpoint":config["endpoint"]})
    }
    fn reviewed_config(&self, config: Value) -> Value {
        self.ok(
            Method::PUT,
            "/api/config/reviewed",
            json!({"expected":self.expected(),"config":config}),
        )
    }
    fn preference_row(&self) -> Option<String> {
        self.app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                [runtime_preferences::SETTINGS_KEY],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }
    async fn drain(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.app().tasks.drain())
            .await
            .expect("registered synthetic setup work did not drain");
    }
    async fn close(&mut self) {
        let app = self.app.take().unwrap();
        app.acceleration.begin_shutdown();
        app.native.begin_shutdown();
        tokio::time::timeout(Duration::from_secs(10), app.tasks.drain())
            .await
            .expect("synthetic task cleanup did not finish");
        app.native.shutdown().await.unwrap();
        let owned = Arc::try_unwrap(app)
            .unwrap_or_else(|_| panic!("a synthetic router or worker retained the app"));
        storage::close(owned.db.into_inner().unwrap())
            .unwrap_or_else(|_| panic!("synthetic SQLCipher connection failed to close"));
    }
    async fn restart(&mut self) {
        self.close().await;
        let db =
            storage::unlock(&self.path, &storage::Secret::new(SECRET.into()).unwrap()).unwrap();
        self.app = Some(app_from_connection(db).unwrap());
    }
}

/// Every route called here either responds immediately or registers background
/// work before responding. Polling exactly once lets the current-thread tests
/// exercise busy guards while the newly registered inspect worker is still
/// queued; there is no scheduler-speed or arbitrary-sleep dependency.
fn request(app: Arc<App>, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
    let request = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:18473")
        .header("origin", "http://127.0.0.1:18473")
        .header("content-type", "application/json")
        .body(if input.is_null() {
            Body::empty()
        } else {
            Body::from(input.to_string())
        })
        .unwrap();
    let response = router(app)
        .oneshot(request)
        .now_or_never()
        .unwrap_or_else(|| {
            panic!("{path} unexpectedly waited instead of responding/registering work")
        })
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .now_or_never()
        .expect("an in-memory synthetic response unexpectedly waited")
        .unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"non_json":String::from_utf8_lossy(&bytes)}));
    (status, body)
}

fn intent() -> Value {
    json!({"mode":"auto","preference":"balanced","capability":"text_chat","local_fallback":"ask","overrides":{}})
}
fn flattened(expected: Value, fields: Value) -> Value {
    let mut object = fields.as_object().unwrap().clone();
    object.extend(expected.as_object().unwrap().clone());
    Value::Object(object)
}
fn assert_status(actual: (StatusCode, Value), expected: StatusCode) {
    assert_eq!(actual.0, expected, "response: {}", actual.1);
}
fn assert_no_runtime(fixture: &Fixture) {
    assert!(fixture.app().native.snapshot().is_none());
    assert!(fixture.get(BASE)["inventory"].is_null());
    assert!(fixture.get("/api/native/status")["runtime"].is_null());
}

#[tokio::test(flavor = "current_thread")]
async fn acceleration_defaults_cas_encrypted_restart_and_reset_preserve_other_settings() {
    let mut f = Fixture::new();
    let status = f.get(BASE);
    let initial = &status["preferences"];
    assert_eq!(initial["revision"], 0);
    assert_eq!(initial["intent"]["mode"], "auto");
    assert_eq!(initial["intent"]["preference"], "balanced");
    assert_eq!(initial["intent"]["capability"], "text_chat");
    assert_eq!(initial["intent"]["local_fallback"], "ask");
    for field in ["inventory", "selection", "prepared", "preview", "operation"] {
        assert!(status[field].is_null());
    }
    assert_eq!(f.preference_row(), None, "reading defaults must not write");
    f.reviewed_config(json!({"provider":"openai","endpoint":"http://127.0.0.1:9/v1","model":"synthetic-model-choice"}));
    f.app().db.lock().unwrap().execute_batch(
        "INSERT INTO settings VALUES('synthetic-model-record','model-record-preserved'),('synthetic-source-record','source-record-preserved')",
    ).unwrap();
    let chosen = json!({"mode":"manual","preference":"quiet","capability":"text_chat","local_fallback":"never","overrides":{"backend":"llama_cpu","device_id":"synthetic-cpu","context_tokens":4096,"threads":2,"mmap":false}});
    let saved = f.ok(
        Method::PUT,
        &format!("{BASE}/preferences"),
        json!({"expected_revision":0,"intent":chosen}),
    );
    assert_eq!(saved["preferences"]["revision"], 1);
    assert_eq!(saved["preferences"]["intent"]["overrides"]["mmap"], false);
    let raw = f.preference_row().unwrap();
    assert_status(
        f.call(
            Method::PUT,
            &format!("{BASE}/preferences"),
            json!({"expected_revision":0,"intent":intent()}),
        ),
        StatusCode::CONFLICT,
    );
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/preferences/reset"),
            json!({"expected_revision":0}),
        ),
        StatusCode::CONFLICT,
    );
    assert_eq!(f.preference_row().as_deref(), Some(raw.as_str()));
    let old_connection_revision = f.get("/api/config")["revision"].clone();
    f.restart().await;
    assert_eq!(f.get(BASE)["preferences"], saved["preferences"]);
    assert_ne!(f.get("/api/config")["revision"], old_connection_revision);
    assert_eq!(f.get("/api/config")["model"], "synthetic-model-choice");
    assert_no_runtime(&f);
    let reset = f.ok(
        Method::POST,
        &format!("{BASE}/preferences/reset"),
        json!({"expected_revision":1}),
    );
    let preferences = &reset["preferences"];
    assert_eq!(preferences["revision"], 2);
    assert_eq!(preferences["intent"]["mode"], "auto");
    assert_eq!(preferences["intent"]["preference"], "quiet");
    assert_eq!(preferences["intent"]["local_fallback"], "never");
    assert!(preferences["intent"]["overrides"]
        .as_object()
        .unwrap()
        .values()
        .all(Value::is_null));
    for (key, expected) in [
        ("synthetic-model-record", "model-record-preserved"),
        ("synthetic-source-record", "source-record-preserved"),
    ] {
        let retained: String = f
            .app()
            .db
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained, expected);
    }
    assert_eq!(f.get("/api/config")["model"], "synthetic-model-choice");
    f.close().await;
    let disk = std::fs::read(&f.path).unwrap();
    assert!(!disk.starts_with(b"SQLite format 3"));
    for plaintext in [
        b"runtime_preferences".as_slice(),
        b"source-record-preserved".as_slice(),
    ] {
        assert!(!disk.windows(plaintext.len()).any(|w| w == plaintext));
    }
}

/// Fixture-only persisted success evidence. This does not claim that the test
/// loaded a model: it seeds the passive record needed to exercise HTTP rollback.
fn seed_success(f: &Fixture, expected_revision: u64) {
    let app = f.app();
    let mut db = app.db.lock().unwrap();
    let current = runtime_preferences::read(&db).unwrap();
    let tx = db.transaction().unwrap();
    runtime_preferences::commit_selected_success(
        &tx,
        expected_revision,
        &current.intent,
        &runtime_preferences::SelectedSuccessRecord {
            model_sha256: "a".repeat(64),
            executable_sha256: "b".repeat(64),
            requested_backend: runtime_preferences::SelectedBackend::Cpu,
            requested_device: None,
            context_tokens: 2048,
            output_tokens: 256,
            threads: 4,
            gpu_layers: Some(0),
            evidence: runtime_preferences::SelectedEvidence::RuntimeReadyPlacementUnknown,
        },
    )
    .unwrap();
    tx.commit().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn acceleration_rollback_restores_retained_intent_without_runtime_or_config_changes() {
    let mut f = Fixture::new();
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/preferences/rollback"),
            json!({"expected_revision":0}),
        ),
        StatusCode::CONFLICT,
    );
    assert_eq!(f.preference_row(), None);
    seed_success(&f, 0);
    let mut quiet = intent();
    quiet["preference"] = json!("quiet");
    f.ok(
        Method::PUT,
        &format!("{BASE}/preferences"),
        json!({"expected_revision":1,"intent":quiet}),
    );
    seed_success(&f, 2);
    let working = f.get(BASE)["preferences"].clone();
    assert_eq!(working["revision"], 3);
    let config = f.get("/api/config");
    f.restart().await;
    let current_connection = f.get("/api/config");
    let rolled = f.ok(
        Method::POST,
        &format!("{BASE}/preferences/rollback"),
        json!({"expected_revision":3}),
    );
    assert_eq!(rolled["runtime_restarted"], false);
    assert_eq!(rolled["preferences"]["revision"], 4);
    assert_eq!(rolled["preferences"]["intent"]["preference"], "balanced");
    assert_eq!(
        rolled["preferences"]["last_successful"],
        working["last_successful"]
    );
    assert_eq!(
        rolled["preferences"]["previous_successful"],
        working["previous_successful"]
    );
    assert_eq!(f.get("/api/config"), current_connection);
    assert_eq!(current_connection["model"], config["model"]);
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/preferences/rollback"),
            json!({"expected_revision":3}),
        ),
        StatusCode::CONFLICT,
    );
    assert_no_runtime(&f);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn acceleration_malformed_inputs_and_unknown_saved_versions_never_reset_preferences() {
    let mut f = Fixture::new();
    let before = f.get(BASE)["preferences"].clone();
    for (field, value, expected) in [
        (
            "mode",
            json!("automatic_remote"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "capability",
            json!("unsupported_capability"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "local_fallback",
            json!("cloud"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "overrides",
            json!({"backend":"invented_backend"}),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("overrides", json!({"threads":0}), StatusCode::BAD_REQUEST),
        (
            "overrides",
            json!({"context_tokens":1_048_577}),
            StatusCode::BAD_REQUEST,
        ),
        ("mode", json!("manual"), StatusCode::BAD_REQUEST),
        ("execute_now", json!(true), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let mut invalid = intent();
        invalid[field] = value;
        assert_status(
            f.call(
                Method::PUT,
                &format!("{BASE}/preferences"),
                json!({"expected_revision":0,"intent":invalid}),
            ),
            expected,
        );
        assert_eq!(f.get(BASE)["preferences"], before);
        assert_eq!(f.preference_row(), None);
    }
    assert_status(
        f.call(
            Method::PUT,
            &format!("{BASE}/preferences"),
            json!({"intent":intent()}),
        ),
        StatusCode::UNPROCESSABLE_ENTITY,
    );
    let saved = f.ok(
        Method::PUT,
        &format!("{BASE}/preferences"),
        json!({"expected_revision":0,"intent":intent()}),
    );
    let mut future = saved["preferences"].clone();
    future["format_version"] = json!(99);
    for raw in [future.to_string(), "{broken runtime preferences".into()] {
        f.app()
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE settings SET value=?1 WHERE key=?2",
                params![raw, runtime_preferences::SETTINGS_KEY],
            )
            .unwrap();
        assert_status(f.call(Method::GET, BASE, Value::Null), StatusCode::CONFLICT);
        for suffix in ["reset", "rollback"] {
            assert_status(
                f.call(
                    Method::POST,
                    &format!("{BASE}/preferences/{suffix}"),
                    json!({"expected_revision":1}),
                ),
                StatusCode::CONFLICT,
            );
        }
        assert_status(
            f.call(
                Method::PUT,
                &format!("{BASE}/preferences"),
                json!({"expected_revision":1,"intent":intent()}),
            ),
            StatusCode::CONFLICT,
        );
        assert_eq!(f.preference_row().as_deref(), Some(raw.as_str()));
    }
    assert!(f.app().native.snapshot().is_none());
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn reviewed_connection_and_auth_reject_stale_changes_without_losing_newer_choices() {
    let mut f = Fixture::new();
    let original = f.expected();
    let first =
        json!({"provider":"openai","endpoint":"http://127.0.0.1:9/v1","model":"synthetic-first"});
    f.ok(
        Method::PUT,
        "/api/config/reviewed",
        json!({"expected":original,"config":first}),
    );
    let first_expected = f.expected();
    f.ok(
        Method::PUT,
        "/api/session-auth/reviewed",
        json!({"expected":first_expected,"token":"synthetic-token-one"}),
    );
    let token_one_expected = f.expected();
    f.ok(
        Method::PUT,
        "/api/session-auth/reviewed",
        json!({"expected":token_one_expected,"token":"synthetic-token-two"}),
    );
    let latest = f.get("/api/config");
    for stale in [&original, &first_expected, &token_one_expected] {
        assert_status(
            f.call(
                Method::PUT,
                "/api/config/reviewed",
                json!({"expected":stale,"config":{"provider":"mock","endpoint":"","model":"old"}}),
            ),
            StatusCode::CONFLICT,
        );
        assert_status(
            f.call(
                Method::PUT,
                "/api/session-auth/reviewed",
                json!({"expected":stale,"token":"synthetic-stale-token"}),
            ),
            StatusCode::CONFLICT,
        );
        assert_status(
            f.call(Method::DELETE, "/api/session-auth/reviewed", stale.clone()),
            StatusCode::CONFLICT,
        );
    }
    assert_eq!(f.get("/api/config"), latest);
    {
        let app = f.app();
        let auth = app.auth.lock().unwrap();
        assert_eq!(
            auth.secret.as_ref().unwrap().bearer.to_str().unwrap(),
            "Bearer synthetic-token-two"
        );
        assert!(auth.secret.as_ref().unwrap().bearer.is_sensitive());
    }
    let stale_other_target = f.expected();
    f.reviewed_config(json!({"provider":"ollama","endpoint":"http://127.0.0.1:10","model":"synthetic-newer-target"}));
    assert_eq!(f.get("/api/session-auth")["present"], false);
    f.ok(
        Method::PUT,
        "/api/session-auth/reviewed",
        json!({"expected":f.expected(),"token":"synthetic-new-target-token"}),
    );
    let new_target = f.get("/api/config");
    assert_status(
        f.call(
            Method::DELETE,
            "/api/session-auth/reviewed",
            stale_other_target.clone(),
        ),
        StatusCode::CONFLICT,
    );
    assert_status(
        f.call(
            Method::PUT,
            "/api/session-auth/reviewed",
            json!({"expected":stale_other_target,"token":"synthetic-old-target-token"}),
        ),
        StatusCode::CONFLICT,
    );
    assert_eq!(f.get("/api/config"), new_target);
    {
        let app = f.app();
        let auth = app.auth.lock().unwrap();
        assert_eq!(
            auth.secret.as_ref().unwrap().bearer.to_str().unwrap(),
            "Bearer synthetic-new-target-token"
        );
        let db = app.db.lock().unwrap();
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM settings WHERE value LIKE '%synthetic-%token%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    assert!(!new_target
        .to_string()
        .contains("synthetic-new-target-token"));
    f.restart().await;
    assert_eq!(f.get("/api/config")["model"], "synthetic-newer-target");
    assert_eq!(f.get("/api/session-auth")["present"], false);
    assert_no_runtime(&f);
    f.close().await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct SelectedFiles {
    executable: PathBuf,
    model: PathBuf,
    marker: PathBuf,
    executable_bytes: Vec<u8>,
    model_bytes: Vec<u8>,
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
impl SelectedFiles {
    fn new(root: &std::path::Path) -> Self {
        let source = root.join("synthetic-tripwire.c");
        let executable = root.join("synthetic-tripwire");
        let marker = root.join("synthetic-tripwire.executed");
        std::fs::write(
            &source,
            r#"#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    (void)argc;
    char path[8192];
    if (snprintf(path, sizeof(path), "%s.executed", argv[0]) >= (int)sizeof(path)) return 91;
    FILE *f = fopen(path, "wb");
    if (!f) return 92;
    fputs("synthetic executable was invoked", f);
    fclose(f);
    return 37;
}
"#,
        )
        .unwrap();
        let compiled = std::process::Command::new("cc")
            .arg("-std=c99")
            .arg("-O0")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("the owned native tripwire fixture requires the build C compiler");
        assert!(
            compiled.status.success(),
            "synthetic tripwire fixture compilation failed"
        );
        // Positive control outside the inspected HTTP operation: demonstrate
        // that ANY invocation, including --help, leaves the marker behind.
        let control = std::process::Command::new(&executable)
            .arg("--help")
            .status()
            .unwrap();
        assert_eq!(control.code(), Some(37));
        assert!(marker.is_file());
        std::fs::remove_file(&marker).unwrap();
        let model = root.join("synthetic-metadata-only.gguf");
        let mut bytes = b"GGUF".to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes()); // no tensors; never used for inference
        bytes.extend(3u64.to_le_bytes());
        fn string(bytes: &mut Vec<u8>, value: &str) {
            bytes.extend((value.len() as u64).to_le_bytes());
            bytes.extend(value.as_bytes());
        }
        string(&mut bytes, "general.architecture");
        bytes.extend(8u32.to_le_bytes());
        string(&mut bytes, "llama");
        for (key, value) in [
            ("llama.context_length", 4096u32),
            ("llama.block_count", 2u32),
        ] {
            string(&mut bytes, key);
            bytes.extend(4u32.to_le_bytes());
            bytes.extend(value.to_le_bytes());
        }
        std::fs::write(&model, &bytes).unwrap();
        Self {
            executable_bytes: std::fs::read(&executable).unwrap(),
            model_bytes: bytes,
            executable,
            model,
            marker,
        }
    }
    fn input(&self, f: &Fixture, revision: u64) -> Value {
        flattened(
            f.expected(),
            json!({
                "executable":{"path":self.executable,"source":"owned synthetic native execution tripwire","source_revision":"synthetic-v1"},
                "model":{"path":self.model,"source":"synthetic metadata-only GGUF fixture","source_revision":"synthetic-v1"},
                "model_id":"synthetic-metadata-only","expected_revision":revision
            }),
        )
    }
    fn unchanged_and_unexecuted(&self) {
        assert!(
            !self.marker.exists(),
            "the read-only/rejected setup operation executed the fixture"
        );
        assert_eq!(
            std::fs::read(&self.executable).unwrap(),
            self.executable_bytes
        );
        assert_eq!(std::fs::read(&self.model).unwrap(), self.model_bytes);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "current_thread")]
async fn inspect_is_read_only_and_probe_and_apply_require_exact_ids_and_confirmation() {
    use sha2::{Digest, Sha256};
    let mut f = Fixture::new();
    let files = SelectedFiles::new(&f.root);
    let started = f.ok(Method::POST, &format!("{BASE}/inspect"), files.input(&f, 0));
    assert!(
        started["operation_id"].is_string(),
        "flattened Expected must deserialize"
    );
    f.drain().await;
    let status = f.get(BASE);
    assert_eq!(status["operation"]["id"], started["operation_id"]);
    assert_eq!(status["operation"]["status"], "succeeded", "{status}");
    let selection = status["selection"].clone();
    assert_eq!(selection["files"]["inference_verified"], false);
    assert_eq!(selection["files"]["metadata"]["context_tokens"], 4096);
    assert_eq!(
        selection["files"]["executable"]["sha256"],
        Sha256::digest(&files.executable_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(
        selection["files"]["model"]["sha256"],
        Sha256::digest(&files.model_bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(
        selection["probe"]["commands"],
        json!(["--help", "--list-devices"])
    );
    assert!(status["prepared"].is_null());
    assert!(status["preview"].is_null());
    files.unchanged_and_unexecuted();
    assert_no_runtime(&f);
    let valid_shape = flattened(
        f.expected(),
        json!({"selection_id":selection["id"],"probe_id":selection["probe"]["id"],"expected_revision":0,"confirm_probe":false}),
    );
    assert_status(
        f.call(Method::POST, &format!("{BASE}/plan"), valid_shape.clone()),
        StatusCode::BAD_REQUEST,
    );
    for field in ["selection_id", "probe_id"] {
        let mut wrong = valid_shape.clone();
        wrong["confirm_probe"] = json!(true);
        wrong[field] = json!("not-the-reviewed-identity");
        assert_status(
            f.call(Method::POST, &format!("{BASE}/plan"), wrong),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            f.get(BASE)["selection"]["probe"]["id"],
            selection["probe"]["id"],
            "rejected requests must not consume another reviewed probe"
        );
    }
    let mut missing = valid_shape.clone();
    missing.as_object_mut().unwrap().remove("confirm_probe");
    assert_status(
        f.call(Method::POST, &format!("{BASE}/plan"), missing),
        StatusCode::UNPROCESSABLE_ENTITY,
    );
    for confirm in [false, true] {
        assert_status(
            f.call(
                Method::POST,
                &format!("{BASE}/apply"),
                flattened(
                    f.expected(),
                    json!({"plan_id":"invented-plan","expected_revision":0,"confirm":confirm}),
                ),
            ),
            StatusCode::BAD_REQUEST,
        );
    }
    let mut hidden_flags = valid_shape.clone();
    hidden_flags["argv"] = json!(["--not-reviewed"]);
    assert_status(
        f.call(Method::POST, &format!("{BASE}/plan"), hidden_flags),
        StatusCode::UNPROCESSABLE_ENTITY,
    );
    f.reviewed_config(json!({"provider":"mock","endpoint":"","model":"unused"}));
    let mut stale = valid_shape;
    stale["confirm_probe"] = json!(true);
    assert_status(
        f.call(Method::POST, &format!("{BASE}/plan"), stale),
        StatusCode::CONFLICT,
    );
    assert_status(f.call(Method::POST, &format!("{BASE}/plan"), flattened(f.expected(), json!({"selection_id":selection["id"],"probe_id":selection["probe"]["id"],"expected_revision":0,"confirm_probe":true}))), StatusCode::BAD_REQUEST);
    files.unchanged_and_unexecuted();
    f.restart().await;
    for field in ["inventory", "selection", "preview", "prepared", "operation"] {
        assert!(f.get(BASE)[field].is_null());
    }
    files.unchanged_and_unexecuted();
    assert_no_runtime(&f);
    f.close().await;
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "current_thread")]
async fn running_setup_blocks_mutations_until_exact_cancelled_operation_drains() {
    let mut f = Fixture::new();
    let files = SelectedFiles::new(&f.root);
    f.reviewed_config(json!({"provider":"openai","endpoint":"http://127.0.0.1:9/v1","model":"synthetic-preserved"}));
    f.ok(
        Method::PUT,
        "/api/session-auth/reviewed",
        json!({"expected":f.expected(),"token":"synthetic-busy-preserved-token"}),
    );
    let config = f.get("/api/config");
    let expected = f.expected();
    let started = f.ok(Method::POST, &format!("{BASE}/inspect"), files.input(&f, 0));
    assert_eq!(f.get(BASE)["operation"]["status"], "running");
    // No await has occurred since registration: the inspection worker is still
    // queued. These are deterministic admission guards, not a timing race.
    for (method, path, input) in [
        (
            Method::PUT,
            format!("{BASE}/preferences"),
            json!({"expected_revision":0,"intent":intent()}),
        ),
        (
            Method::POST,
            format!("{BASE}/preferences/reset"),
            json!({"expected_revision":0}),
        ),
        (
            Method::POST,
            format!("{BASE}/preferences/rollback"),
            json!({"expected_revision":0}),
        ),
        (
            Method::POST,
            format!("{BASE}/inventory"),
            json!({"expected_revision":0}),
        ),
        (Method::POST, format!("{BASE}/inspect"), files.input(&f, 0)),
        (
            Method::POST,
            format!("{BASE}/plan"),
            flattened(
                expected.clone(),
                json!({"selection_id":"unavailable","probe_id":"unavailable","confirm_probe":true,"expected_revision":0}),
            ),
        ),
        (
            Method::POST,
            format!("{BASE}/apply"),
            flattened(
                expected.clone(),
                json!({"plan_id":"unavailable","confirm":true,"expected_revision":0}),
            ),
        ),
        (
            Method::PUT,
            "/api/config/reviewed".into(),
            json!({"expected":expected,"config":{"provider":"mock","endpoint":"","model":"stale"}}),
        ),
        (
            Method::PUT,
            "/api/session-auth/reviewed".into(),
            json!({"expected":expected,"token":"synthetic-busy-replacement"}),
        ),
        (
            Method::DELETE,
            "/api/session-auth/reviewed".into(),
            expected.clone(),
        ),
    ] {
        assert_status(f.call(method, &path, input), StatusCode::CONFLICT);
    }
    assert_eq!(f.get("/api/config"), config);
    assert_eq!(f.preference_row(), None);
    {
        let app = f.app();
        let auth = app.auth.lock().unwrap();
        assert_eq!(
            auth.secret.as_ref().unwrap().bearer.to_str().unwrap(),
            "Bearer synthetic-busy-preserved-token"
        );
    }
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/cancel"),
            json!({"operation_id":"another-operation"}),
        ),
        StatusCode::BAD_REQUEST,
    );
    f.ok(
        Method::POST,
        &format!("{BASE}/cancel"),
        json!({"operation_id":started["operation_id"]}),
    );
    assert_eq!(f.get(BASE)["operation"]["status"], "running");
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/preferences/reset"),
            json!({"expected_revision":0}),
        ),
        StatusCode::CONFLICT,
    );
    f.drain().await;
    let finished = f.get(BASE);
    assert_eq!(finished["operation"]["id"], started["operation_id"]);
    assert_eq!(finished["operation"]["status"], "cancelled");
    assert!(finished["selection"].is_null());
    assert_status(
        f.call(
            Method::POST,
            &format!("{BASE}/cancel"),
            json!({"operation_id":started["operation_id"]}),
        ),
        StatusCode::BAD_REQUEST,
    );
    let accepted = f.ok(
        Method::PUT,
        &format!("{BASE}/preferences"),
        json!({"expected_revision":0,"intent":intent()}),
    );
    assert_eq!(accepted["preferences"]["revision"], 1);
    files.unchanged_and_unexecuted();
    assert_no_runtime(&f);
    f.close().await;
}
