//! Explicit native MLX setup. Hash-only preparation and exact reviewed loading
//! are separate actions; none runs during startup or passive settings polling.
use crate::{
    bridge::{Bridge, Config, Result, Transport},
    settings::SelectedFile,
};
use serde_json::{json, Value};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    #[default]
    ModelDefault,
    Disabled,
}
#[derive(Clone, Debug)]
pub struct MlxReview {
    pub config: Config,
    pub settings_revision: u64,
    pub prepared: Value,
}
#[derive(Clone, Debug)]
pub struct MlxSelection {
    pub worker: SelectedFile,
    pub directory: String,
    pub model_id: String,
    pub capability: String,
    pub sampling: Value,
    pub thinking_mode: ThinkingMode,
}
/// Host capability is independent of the persisted settings format. Legacy
/// saved preferences do not hide controls supported by a newer host.
pub fn supports_thinking_control(status: &Value) -> bool {
    status["thinking_modes"].as_array().is_some_and(|modes| {
        modes.iter().any(|v| v == "model_default") && modes.iter().any(|v| v == "disabled")
    })
}
/// Sampling fields are declared f32 by the runtime contract. Format that value
/// with Rust's shortest round-trip representation instead of showing the f64
/// tail introduced by JSON serialization. This never changes the stored value
/// or the exact prepared-plan comparison.
pub fn sampling_value_text(value: &Value) -> String {
    value
        .as_f64()
        .map(|number| number as f32)
        .filter(|number| number.is_finite())
        .map(|number| number.to_string())
        .unwrap_or_else(|| "Unavailable".into())
}
fn expected(c: &Config) -> Value {
    json!({"revision":c.revision,"provider":c.provider,"endpoint":c.endpoint})
}
impl<T: Transport> Bridge<T> {
    pub fn refresh_mlx(&mut self) -> Result<()> {
        self.check_session()?;
        let value = self.get("/api/mlx/status")?;
        self.check_session()?;
        self.view.mlx = value;
        self.refresh_configuration()
    }
    pub fn mlx_review(&self) -> Result<MlxReview> {
        Ok(MlxReview {
            config: self.view.config.clone(),
            settings_revision: self.view.mlx["settings"]["revision"]
                .as_u64()
                .ok_or("Refresh MLX settings first.")?,
            prepared: self.view.mlx["prepared"].clone(),
        })
    }
    fn mlx_current(&mut self, r: &MlxReview) -> Result<()> {
        self.check_session()?;
        if r.config != self.view.config
            || Some(r.settings_revision) != self.view.mlx["settings"]["revision"].as_u64()
        {
            return Err("MLX review is stale. Refresh and review again.".into());
        }
        Ok(())
    }
    pub fn prepare_mlx(&mut self, r: &MlxReview, s: MlxSelection) -> Result<()> {
        self.mlx_current(r)?;
        if s.thinking_mode == ThinkingMode::Disabled && !supports_thinking_control(&self.view.mlx) {
            return Err("This backend does not expose reviewed thinking control. Keep Model default or update the backend.".into());
        }
        let mut body = json!({"worker":{"path":s.worker.path,"source":s.worker.source,"source_revision":s.worker.source_revision},"model_directory":s.directory,"model_id":s.model_id,"capability":s.capability,"sampling":s.sampling,"settings_revision":r.settings_revision,"expected":expected(&r.config)});
        if supports_thinking_control(&self.view.mlx) {
            body["thinking_mode"] = json!(s.thinking_mode);
        }
        self.transport.request(
            "POST",
            "/api/mlx/prepare",
            self.session.as_deref(),
            Some(body),
        )?;
        self.refresh_mlx()
    }
    pub fn load_mlx(&mut self, r: &MlxReview) -> Result<()> {
        self.mlx_current(r)?;
        if r.prepared.is_null() || r.prepared != self.view.mlx["prepared"] {
            return Err(
                "The reviewed MLX plan changed. Review the new plan before loading.".into(),
            );
        }
        let id = r.prepared["plan"]["id"]
            .as_str()
            .ok_or("MLX prepared plan is missing.")?;
        let mut body = json!({"plan_id":id,"settings_revision":r.settings_revision,"confirm":true,"expected":expected(&r.config)});
        if supports_thinking_control(&self.view.mlx)
            || r.prepared.get("review_id").is_some()
            || r.prepared.get("thinking_mode").is_some()
        {
            let review_id=r.prepared["review_id"].as_str().filter(|s|s.len()==64&&s.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))).ok_or("The prepared MLX settings lack their exact review identity. Inspect them again.")?;
            serde_json::from_value::<ThinkingMode>(r.prepared["thinking_mode"].clone())
                .map_err(|_| "The prepared thinking choice is unsupported.")?;
            body["review_id"] = json!(review_id);
        }
        self.transport
            .request("POST", "/api/mlx/load", self.session.as_deref(), Some(body))?;
        self.refresh_mlx()
    }
    pub fn cancel_mlx(&mut self, c: &Config, operation: &Value) -> Result<()> {
        self.check_session()?;
        if c != &self.view.config
            || operation["id"] != self.view.mlx["operation"]["id"]
            || operation["status"] != "running"
            || operation["kind"] == "unload"
        {
            return Err("MLX operation changed or cannot be cancelled.".into());
        }
        self.transport.request(
            "POST",
            "/api/mlx/cancel",
            self.session.as_deref(),
            Some(json!({"operation_id":operation["id"],"expected":expected(c)})),
        )?;
        self.refresh_mlx()
    }
    pub fn unload_mlx(&mut self, c: &Config, runtime: &Value) -> Result<()> {
        self.check_session()?;
        if c != &self.view.config || runtime.is_null() || runtime != &self.view.mlx["runtime"] {
            return Err(
                "The owned MLX worker changed. Refresh its status before unloading.".into(),
            );
        }
        self.transport.request("POST","/api/mlx/unload",self.session.as_deref(),Some(json!({"session_id":runtime["session_id"],"runtime_revision":runtime["revision"],"expected":expected(c)})))?;
        self.refresh_mlx()
    }
    pub fn raw_mlx(
        &mut self,
        c: &Config,
        runtime: &Value,
        prompt: String,
        sampling: Value,
    ) -> Result<()> {
        self.check_session()?;
        if c != &self.view.config
            || runtime != &self.view.mlx["runtime"]
            || runtime.is_null()
            || self.view.mlx["loaded"]["capability"] != "raw_completion"
        {
            return Err("Select a reviewed raw-completion MLX load first.".into());
        }
        self.transport.request("POST","/api/mlx/completions",self.session.as_deref(),Some(json!({"session_id":runtime["session_id"],"runtime_revision":runtime["revision"],"prompt":prompt,"sampling":sampling,"confirm":true,"expected":expected(c)})))?;
        self.refresh_mlx()
    }
}
