use serde_json::{json, Value};
use smith_native_bridge::{
    bridge::{Result, Transport},
    fixture::{Fixture, FixtureTransport},
};
use std::sync::{Arc, Mutex};
#[derive(Clone)]
pub struct Control(pub Arc<Mutex<State>>);
pub struct State {
    pub config: Value,
    pub setup: Value,
    pub native: Value,
    pub mlx: Value,
    pub catalog: Value,
    pub calls: Vec<(String, Value)>,
    pub discoveries: usize,
    pub loads: usize,
}
pub struct SettingsFixture {
    base: FixtureTransport,
    pub control: Control,
}
impl SettingsFixture {
    pub fn new() -> Self {
        let fixture = Fixture::start().unwrap();
        Self {
            base: fixture.transport(),
            control: Control(Arc::new(Mutex::new(State {
                config: json!({"revision":"settings-1","provider":"mock","endpoint":"","model":"smith-demo","tested":true}),
                setup: json!({"preferences":{"revision":0,"intent":{"mode":"auto","preference":"balanced","capability":"text_chat","local_fallback":"ask","overrides":{"backend":null,"context_tokens":null,"threads":null,"gpu_layers":null,"device_id":null,"output_tokens":null,"prefill_threads":null,"batch_tokens":null,"microbatch_tokens":null,"host_budget_bytes":null,"device_budget_bytes":null,"headroom_bytes":null,"kv_type":null,"flash_attention":null,"mmap":null,"mlock":null}}},"selection":null,"prepared":null,"preview":null,"inventory":null,"operation":null}),
                native: json!({"runtime":null,"operation":null}),
                mlx: json!({"thinking_modes":["model_default","disabled"],"settings":{"format_version":2,"revision":0,"thinking_mode":"model_default","capability":"chat","sampling":{"max_tokens":128,"temperature":0.6,"top_p":0.9}},"prepared":null,"loaded":null,"runtime":null,"operation":null}),
                catalog: json!({"catalog":{"revision":0,"sha256":null},"entries":[],"selected_for_review":null,"operations":[],"prepared":null,"inspection":"stored_metadata_only","automatic_load":false,"hints_applied":false}),
                calls: vec![],
                discoveries: 0,
                loads: 0,
            }))),
        }
    }
}
fn expected_matches(state: &State, body: &Value) -> bool {
    ["revision", "provider", "endpoint"]
        .iter()
        .all(|key| state.config[key] == body[key])
}
fn advance(state: &mut State) {
    let n = state.config["revision"]
        .as_str()
        .unwrap()
        .split('-')
        .next_back()
        .unwrap()
        .parse::<u64>()
        .unwrap()
        + 1;
    state.config["revision"] = json!(format!("settings-{n}"));
}
impl Transport for SettingsFixture {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut s = self.control.0.lock().unwrap();
        let b = body.clone().unwrap_or(Value::Null);
        if path == "/api/config" {
            return Ok(s.config.clone());
        }
        if path.starts_with("/api/models?") {
            return Ok(
                json!({"revision":s.config["revision"],"provider":s.config["provider"],"endpoint":s.config["endpoint"],"models":if s.config["provider"]=="mock"{vec![json!({"id":"smith-demo"})]}else if s.config["tested"]==true {vec![json!({"id":"fixture-model"})]}else{vec![]}}),
            );
        }
        if path == "/api/acceleration" {
            return Ok(s.setup.clone());
        }
        if path == "/api/native/status" {
            return Ok(s.native.clone());
        }
        if path == "/api/mlx/status" {
            return Ok(s.mlx.clone());
        }
        if path == "/api/local-catalog" {
            return Ok(s.catalog.clone());
        }
        if path == "/api/local-catalog/setup" {
            if b["adapter"] != "mlx"
                || b["review_id"] != s.catalog["prepared"]["id"]
                || !expected_matches(&s, &b["settings"]["expected"])
            {
                return Err("409 synthetic catalog setup changed".into());
            }
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"44444444-4444-4444-8444-444444444444"}));
        }
        if path == "/api/mlx/prepare" {
            if !expected_matches(&s, &b["expected"])
                || b["settings_revision"] != s.mlx["settings"]["revision"]
            {
                return Err("409 stale synthetic MLX preparation".into());
            }
            assert!(matches!(
                b["thinking_mode"].as_str(),
                Some("model_default" | "disabled")
            ));
            s.mlx["prepared"] = json!({"plan":{"id":"synthetic-mlx-plan","worker":{"path":b["worker"]["path"]},"model":{"root":b["model_directory"]}},"review_id":"a".repeat(64),"thinking_mode":b["thinking_mode"],"settings_revision":0,"capability":b["capability"],"sampling":b["sampling"]});
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"synthetic-mlx-inspection"}));
        }
        if path == "/api/mlx/load" {
            if !expected_matches(&s, &b["expected"])
                || b["plan_id"] != s.mlx["prepared"]["plan"]["id"]
                || b["review_id"] != s.mlx["prepared"]["review_id"]
                || b["confirm"] != true
            {
                return Err("409 stale synthetic MLX review".into());
            }
            s.mlx["loaded"] = json!({"capability":s.mlx["prepared"]["capability"],"thinking_mode":s.mlx["prepared"]["thinking_mode"],"model_id":"synthetic-only"});
            s.mlx["prepared"] = Value::Null;
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"synthetic-mlx-load-ack"}));
        }
        if path == "/api/config/reviewed" {
            if !expected_matches(&s, &b["expected"]) {
                return Err("409 stale connection".into());
            }
            for key in ["provider", "endpoint", "model"] {
                s.config[key] = b["config"][key].clone();
            }
            s.config["tested"] = json!(false);
            advance(&mut s);
            s.calls.push((path.into(), b));
            return Ok(json!({"ok":true,"revision":s.config["revision"]}));
        }
        if path == "/api/connection/test" {
            if !expected_matches(&s, &b) {
                return Err("409 stale connection".into());
            }
            s.discoveries += 1;
            s.config["tested"] = json!(true);
            s.calls.push((path.into(), b));
            return Ok(
                json!({"revision":s.config["revision"],"provider":s.config["provider"],"endpoint":s.config["endpoint"],"message":"Synthetic connection tested"}),
            );
        }
        if path == "/api/model" {
            if !expected_matches(&s, &b)
                || s.config["tested"] != true
                || b["model"] != "fixture-model"
            {
                return Err("409 stale model".into());
            }
            s.config["model"] = b["model"].clone();
            advance(&mut s);
            s.calls.push((path.into(), b));
            return Ok(s.config.clone());
        }
        if path == "/api/session-auth/reviewed" {
            if !expected_matches(&s, if method == "PUT" { &b["expected"] } else { &b }) {
                return Err("409 stale token".into());
            }
            advance(&mut s);
            s.config["tested"] = json!(false);
            s.calls
                .push((path.into(), json!({"secret_recorded":false})));
            return Ok(json!({"revision":s.config["revision"],"present":method=="PUT"}));
        }
        if path == "/api/acceleration/preferences" {
            if b["expected_revision"] != s.setup["preferences"]["revision"] {
                return Err("409 stale preferences".into());
            }
            s.setup["preferences"]["intent"] = b["intent"].clone();
            let n = s.setup["preferences"]["revision"].as_u64().unwrap() + 1;
            s.setup["preferences"]["revision"] = json!(n);
            s.setup["prepared"] = Value::Null;
            s.calls.push((path.into(), b));
            return Ok(json!({"ok":true}));
        }
        if path == "/api/acceleration/inspect" {
            if !expected_matches(&s, &b)
                || b["expected_revision"] != s.setup["preferences"]["revision"]
            {
                return Err("409 stale inspection".into());
            }
            s.setup["selection"] = json!({"id":"selection-1","model_id":b["model_id"],"files":{"executable":b["executable"],"model":b["model"]},"probe":{"id":"probe-1","commands":["--help","--list-devices"]}});
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"inspect-1"}));
        }
        if path == "/api/acceleration/plan" {
            if !expected_matches(&s, &b)
                || b["expected_revision"] != s.setup["preferences"]["revision"]
                || b["selection_id"] != s.setup["selection"]["id"]
                || b["probe_id"] != s.setup["selection"]["probe"]["id"]
                || b["confirm_probe"] != true
            {
                return Err("409 stale probe".into());
            }
            s.setup["prepared"] = json!({"plan":{"id":"plan-1","arguments":["synthetic-only"]},"settings_revision":s.setup["preferences"]["revision"],"output_tokens":128});
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"probe-operation-1"}));
        }
        if path == "/api/acceleration/apply" {
            if !expected_matches(&s, &b)
                || b["expected_revision"] != s.setup["preferences"]["revision"]
                || b["plan_id"] != s.setup["prepared"]["plan"]["id"]
                || b["confirm"] != true
                || s.loads > 0
            {
                return Err("409 stale or consumed plan".into());
            }
            s.loads += 1;
            s.calls.push((path.into(), b));
            s.native = json!({"operation":{"id":"load-1","status":"succeeded","kind":"load"},"runtime":{"phase":"ready","target":{"owner":"synthetic-owner","generation":1},"revision":3,"model_id":"synthetic-model","observed_backend":"unknown"}});
            return Ok(json!({"operation_id":"load-1"}));
        }
        if path == "/api/native/unload" {
            if !expected_matches(&s, &b)
                || b["owner"] != s.native["runtime"]["target"]["owner"]
                || b["runtime_revision"] != s.native["runtime"]["revision"]
            {
                return Err("409 stale runtime".into());
            }
            s.native = json!({"runtime":null,"operation":null});
            s.calls.push((path.into(), b));
            return Ok(json!({"operation_id":"unload-1"}));
        }
        drop(s);
        self.base.request(method, path, session, body)
    }
}
