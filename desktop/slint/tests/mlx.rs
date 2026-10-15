use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Bridge, Result, Transport},
    mlx::{MlxSelection, ThinkingMode},
    settings::SelectedFile,
};
#[path = "support/settings_fixture.rs"]
mod settings_fixture;
struct MlxFixture {
    base: settings_fixture::SettingsFixture,
    status: Value,
    calls: Vec<(String, Value)>,
}
impl MlxFixture {
    fn new() -> Self {
        Self {
            base: settings_fixture::SettingsFixture::new(),
            status: json!({"thinking_modes":["model_default","disabled"],"settings":{"format_version":2,"thinking_mode":"model_default","revision":0,"sampling":{"max_tokens":128,"temperature":0.6,"top_p":0.9},"capability":"chat"},"prepared":null,"runtime":null,"loaded":null,"operation":null}),
            calls: vec![],
        }
    }
}
impl Transport for MlxFixture {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        if path == "/api/mlx/status" {
            return Ok(self.status.clone());
        }
        if path.starts_with("/api/mlx/") {
            let body = body.unwrap();
            let config = self.base.control.0.lock().unwrap().config.clone();
            for key in ["revision", "provider", "endpoint"] {
                if body["expected"][key] != config[key] {
                    return Err("409 stale expected".into());
                }
            }
            self.calls.push((path.into(), body.clone()));
            match path {
                "/api/mlx/prepare" => {
                    assert!(body.get("execution_kind").is_none());
                    assert!(body["sampling"].get("gpu_layers").is_none());
                    self.status["prepared"] = json!({"plan":{"id":"reviewed-mlx-plan","worker":{"path":body["worker"]["path"],"sha256":"synthetic"},"model":{"root":body["model_directory"],"package_sha256":"synthetic"}},"review_id":"b".repeat(64),"thinking_mode":body["thinking_mode"],"settings_revision":0,"revision":config["revision"],"capability":body["capability"],"sampling":body["sampling"]});
                }
                "/api/mlx/load" => {
                    if body["plan_id"] != self.status["prepared"]["plan"]["id"]
                        || body["confirm"] != true
                        || body["review_id"] != self.status["prepared"]["review_id"]
                    {
                        return Err("409 plan mismatch".into());
                    }
                    self.status["runtime"] = json!({"session_id":"owned-session","revision":1});
                    self.status["loaded"] =
                        json!({"capability":self.status["prepared"]["capability"]});
                    self.status["prepared"] = Value::Null;
                }
                "/api/mlx/unload" => {
                    assert_eq!(body["session_id"], "owned-session");
                    self.status["runtime"] = Value::Null;
                }
                _ => return Err("unexpected MLX request".into()),
            };
            return Ok(json!({"operation_id":"synthetic-operation"}));
        }
        self.base.request(method, path, session, body)
    }
}
#[test]
fn explicit_mlx_review_keeps_separate_hash_load_and_exact_revision() {
    let fixture = MlxFixture::new();
    let control = fixture.base.control.clone();
    let mut b = Bridge::new(fixture);
    b.initialize().unwrap();
    assert!(b.view.mlx.is_null());
    b.refresh_mlx().unwrap();
    let r = b.mlx_review().unwrap();
    b.prepare_mlx(
        &r,
        MlxSelection {
            worker: SelectedFile {
                path: "/synthetic/selected-worker".into(),
                source: "synthetic".into(),
                source_revision: "test".into(),
            },
            directory: "/synthetic/selected-package".into(),
            model_id: "fixture".into(),
            capability: "chat".into(),
            sampling: json!({"max_tokens":8,"temperature":0.0,"top_p":1.0}),
            thinking_mode: ThinkingMode::Disabled,
        },
    )
    .unwrap();
    assert!(b.view.mlx["runtime"].is_null());
    assert!(b.load_mlx(&r).is_err());
    let ready = b.mlx_review().unwrap();
    assert_eq!(ready.prepared["thinking_mode"], "disabled");
    let mut forged = ready.clone();
    forged.prepared["review_id"] = json!("a".repeat(64));
    assert!(b.load_mlx(&forged).is_err());
    b.load_mlx(&ready).unwrap();
    assert!(b.load_mlx(&ready).is_err());
    b.unload_mlx(&b.view.config.clone(), &b.view.mlx["runtime"].clone())
        .unwrap();
    assert!(b.view.mlx["runtime"].is_null());
    let stale = b.mlx_review().unwrap();
    control.0.lock().unwrap().config["revision"] = json!("changed-by-other-tab");
    assert!(b
        .prepare_mlx(
            &stale,
            MlxSelection {
                worker: SelectedFile {
                    path: "/synthetic/worker".into(),
                    source: "fixture".into(),
                    source_revision: "1".into()
                },
                directory: "/synthetic/package".into(),
                model_id: "fixture".into(),
                capability: "chat".into(),
                sampling: json!({"max_tokens":8,"temperature":0.0,"top_p":1.0}),
                thinking_mode: ThinkingMode::ModelDefault
            }
        )
        .is_err());
}

#[test]
fn legacy_saved_settings_do_not_hide_new_host_thinking_capability() {
    let mut fixture = MlxFixture::new();
    fixture.status["settings"]["format_version"] = json!(1);
    fixture.status["settings"]
        .as_object_mut()
        .unwrap()
        .remove("thinking_mode");
    let mut b = Bridge::new(fixture);
    b.initialize().unwrap();
    b.refresh_mlx().unwrap();
    assert!(smith_native_bridge::mlx::supports_thinking_control(
        &b.view.mlx
    ));
    b.prepare_mlx(
        &b.mlx_review().unwrap(),
        MlxSelection {
            worker: SelectedFile {
                path: "/synthetic/worker".into(),
                source: "synthetic".into(),
                source_revision: "test".into(),
            },
            directory: "/synthetic/model".into(),
            model_id: "fixture".into(),
            capability: "chat".into(),
            sampling: json!({"max_tokens":8,"temperature":0.0,"top_p":1.0}),
            thinking_mode: ThinkingMode::Disabled,
        },
    )
    .unwrap();
    assert_eq!(b.view.mlx["settings"]["format_version"], 1);
    assert_eq!(b.view.mlx["prepared"]["thinking_mode"], "disabled");
    assert!(b.view.mlx["runtime"].is_null());
}
