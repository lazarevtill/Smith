//! Native settings actions retain the exact displayed connection and plan.
//! UI fields are separate from these snapshots and are never overwritten by polling.
use crate::bridge::{Bridge, Config, Result, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub setup: Value,
    pub native: Value,
}
#[derive(Clone, Debug)]
pub struct Review {
    pub config: Config,
    pub settings_revision: u64,
    pub selection: Value,
    pub prepared: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectedFile {
    pub path: String,
    pub source: String,
    pub source_revision: String,
}
impl<T: Transport> Bridge<T> {
    pub fn settings_review(&self) -> Result<Review> {
        Ok(Review {
            config: self.view.config.clone(),
            settings_revision: self.view.settings.setup["preferences"]["revision"]
                .as_u64()
                .ok_or("Refresh local setup before reviewing an action.")?,
            selection: self.view.settings.setup["selection"].clone(),
            prepared: self.view.settings.setup["prepared"].clone(),
        })
    }
    fn check_configuration(&self, reviewed: &Config) -> Result<()> {
        if !self.view.available || reviewed != &self.view.config {
            return Err(
                "The displayed connection changed. Refresh and review it before retrying.".into(),
            );
        }
        Ok(())
    }
    fn check_settings(&self, reviewed: &Review) -> Result<()> {
        self.check_configuration(&reviewed.config)?;
        if self.view.settings.setup["preferences"]["revision"].as_u64()
            != Some(reviewed.settings_revision)
        {
            return Err("Local model settings changed. Review a fresh plan.".into());
        }
        Ok(())
    }
    pub fn refresh_configuration(&mut self) -> Result<()> {
        self.check_session()?;
        self.view.config = serde_json::from_value(self.get("/api/config")?)
            .map_err(|_| "Invalid connection response.")?;
        self.refresh_lists()
    }
    pub fn save_connection(
        &mut self,
        reviewed: &Config,
        provider: String,
        endpoint: String,
    ) -> Result<()> {
        self.check_configuration(reviewed)?;
        if !matches!(provider.as_str(), "mock" | "ollama" | "openai" | "managed") {
            return Err("Choose a supported connection type.".into());
        }
        let result = self.request_settings(
            "PUT",
            "/api/config/reviewed",
            json!({
                "expected": expected(reviewed),
                "config":{"provider":provider,"endpoint":endpoint,"model":""}
            }),
        )?;
        self.refresh_configuration()?;
        if result["revision"].as_str() != Some(&self.view.config.revision) {
            return Err(
                "The connection changed again after saving. Review the current destination.".into(),
            );
        }
        self.view.status =
            "Connection saved. Test this destination before selecting a model.".into();
        Ok(())
    }
    pub fn test_connection(&mut self, reviewed: &Config) -> Result<()> {
        self.check_configuration(reviewed)?;
        let result = self.request_settings("POST", "/api/connection/test", expected(reviewed))?;
        if result["revision"].as_str() != Some(&reviewed.revision)
            || result["provider"] != reviewed.provider
            || result["endpoint"] != reviewed.endpoint
        {
            return Err("Connection test returned a different target.".into());
        }
        self.refresh_configuration()?;
        self.view.status = result["message"]
            .as_str()
            .unwrap_or("Connection tested.")
            .into();
        Ok(())
    }
    pub fn session_token(&mut self, reviewed: &Config, token: Option<String>) -> Result<()> {
        self.check_configuration(reviewed)?;
        let result = if let Some(token) = token {
            self.request_settings(
                "PUT",
                "/api/session-auth/reviewed",
                json!({"expected":expected(reviewed),"token":token}),
            )?
        } else {
            self.request_settings("DELETE", "/api/session-auth/reviewed", expected(reviewed))?
        };
        self.refresh_configuration()?;
        if result["revision"].as_str() != Some(&self.view.config.revision) {
            return Err(
                "The connection changed after the token action. Review the current destination."
                    .into(),
            );
        }
        self.view.status = "Session token updated. Test the connection again.".into();
        Ok(())
    }
    pub fn refresh_settings(&mut self) -> Result<()> {
        self.check_session()?;
        let previous = self.view.config.clone();
        self.view.settings.setup = self.get("/api/acceleration")?;
        self.view.settings.native = self.get("/api/native/status")?;
        // Observing a successful load updates the authoritative target, never
        // the unsaved form. Further sends capture this new target explicitly.
        self.view.config = serde_json::from_value(self.get("/api/config")?)
            .map_err(|_| "Invalid connection response.")?;
        if self.view.config != previous {
            self.refresh_lists()?;
        }
        Ok(())
    }
    pub fn save_preferences(&mut self, review: &Review, intent: Value) -> Result<()> {
        self.check_settings(review)?;
        self.request_settings(
            "PUT",
            "/api/acceleration/preferences",
            json!({"expected_revision":review.settings_revision,"intent":intent}),
        )?;
        self.refresh_settings()
    }
    pub fn reset_preferences(&mut self, review: &Review, rollback: bool) -> Result<()> {
        self.check_settings(review)?;
        self.request_settings(
            "POST",
            if rollback {
                "/api/acceleration/preferences/rollback"
            } else {
                "/api/acceleration/preferences/reset"
            },
            json!({"expected_revision":review.settings_revision}),
        )?;
        self.refresh_settings()
    }
    pub fn inventory(&mut self, review: &Review) -> Result<()> {
        self.check_settings(review)?;
        self.request_settings(
            "POST",
            "/api/acceleration/inventory",
            json!({"expected_revision":review.settings_revision}),
        )?;
        self.refresh_settings()
    }
    pub fn inspect_files(
        &mut self,
        review: &Review,
        executable: SelectedFile,
        model: SelectedFile,
        model_id: String,
    ) -> Result<()> {
        self.check_settings(review)?;
        let mut body = expected(&review.config);
        body["expected_revision"] = json!(review.settings_revision);
        body["executable"] = json!(executable);
        body["model"] = json!(model);
        body["model_id"] = json!(model_id);
        self.request_settings("POST", "/api/acceleration/inspect", body)?;
        self.refresh_settings()
    }
    pub fn prepare_plan(&mut self, review: &Review) -> Result<()> {
        self.check_settings(review)?;
        if review.selection.is_null() || review.selection != self.view.settings.setup["selection"] {
            return Err(
                "Selected files changed. Review the new inspection before running checks.".into(),
            );
        }
        let selection = review.selection["id"]
            .as_str()
            .ok_or("Inspect the selected files first.")?;
        let probe = review.selection["probe"]["id"]
            .as_str()
            .ok_or("No qualified probe is available for these files.")?;
        let mut body = expected(&review.config);
        body["selection_id"] = json!(selection);
        body["probe_id"] = json!(probe);
        body["expected_revision"] = json!(review.settings_revision);
        body["confirm_probe"] = json!(true);
        self.request_settings("POST", "/api/acceleration/plan", body)?;
        self.refresh_settings()
    }
    pub fn apply_plan(&mut self, review: &Review) -> Result<()> {
        self.check_settings(review)?;
        if review.prepared.is_null() || review.prepared != self.view.settings.setup["prepared"] {
            return Err(
                "The reviewed load plan changed. Inspect and confirm the current plan.".into(),
            );
        }
        let id = review.prepared["plan"]["id"]
            .as_str()
            .ok_or("There is no reviewed plan to load.")?;
        let mut body = expected(&review.config);
        body["plan_id"] = json!(id);
        body["expected_revision"] = json!(review.settings_revision);
        body["confirm"] = json!(true);
        if !review.prepared["catalog"].is_null() {
            let catalog = review.prepared["catalog"]["id"]
                .as_str()
                .filter(|s| {
                    s.len() == 64
                        && s.bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                .ok_or("The load plan has an invalid saved-model review. Prepare it again.")?;
            body["catalog_review_id"] = json!(catalog);
        }
        self.request_settings("POST", "/api/acceleration/apply", body)?;
        self.refresh_settings()
    }
    pub fn cancel_setup(&mut self, config: &Config, operation: &Value, native: bool) -> Result<()> {
        self.check_configuration(config)?;
        let current = if native {
            &self.view.settings.native["operation"]
        } else {
            &self.view.settings.setup["operation"]
        };
        if current != operation || operation["status"] != "running" || operation["kind"] == "unload"
        {
            return Err("This setup operation is no longer cancellable.".into());
        }
        let mut body = if native { expected(config) } else { json!({}) };
        body["operation_id"] = operation["id"].clone();
        self.request_settings(
            "POST",
            if native {
                "/api/native/cancel"
            } else {
                "/api/acceleration/cancel"
            },
            body,
        )?;
        self.refresh_settings()
    }
    pub fn unload(&mut self, config: &Config, runtime: &Value) -> Result<()> {
        self.check_configuration(config)?;
        if runtime.is_null() || runtime != &self.view.settings.native["runtime"] {
            return Err("The owned model changed. Refresh before unloading.".into());
        }
        let mut body = expected(config);
        body["owner"] = runtime["target"]["owner"].clone();
        body["generation"] = runtime["target"]["generation"].clone();
        body["runtime_revision"] = runtime["revision"].clone();
        body["model_id"] = runtime["model_id"].clone();
        self.request_settings("POST", "/api/native/unload", body)?;
        self.refresh_settings()
    }
    fn request_settings(&mut self, method: &str, path: &str, body: Value) -> Result<Value> {
        self.transport
            .request(method, path, self.session.as_deref(), Some(body))
    }
}
fn expected(config: &Config) -> Value {
    json!({"revision":config.revision,"provider":config.provider,"endpoint":config.endpoint})
}
