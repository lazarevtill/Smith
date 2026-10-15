//! Local setup joins OS inventory, persistent intent and selected-file custody.
//! Readiness and inventory are never represented as measured GPU placement.
use crate::*;
use smith_acceleration as planning;
use smith_managed_runtime as runtime;
#[cfg(test)]
#[path = "../tests/support/acceleration_advice_review.rs"]
mod acceleration_advice_review;

#[derive(Default)]
pub struct AccelerationState {
    inner: Mutex<Inner>,
}
#[derive(Default)]
struct Inner {
    inventory: Option<hardware_inventory::Observation>,
    operation: Option<Operation>,
    selection: Option<Selection>,
    prepared: Option<PreparedSetup>,
    preview: Option<Value>,
    stopping: bool,
    cleanup_unconfirmed: bool,
}
struct Selection {
    id: String,
    model_id: String,
    files: runtime_catalog::CatalogSelection,
    expected: Expected,
    probe: Option<runtime::RuntimeProbePlan>,
    catalog: Option<local_catalog_api::CatalogBinding>,
}
struct PreparedSetup {
    selection_id: String,
    runtime_revision: u64,
    plan: runtime::LoadPlan,
    expected: Expected,
    preferences: native_api::PreparedPreferences,
    inventory: hardware_inventory::Observation,
    device_id: String,
    catalog: Option<local_catalog_api::CatalogBinding>,
}
struct Operation {
    id: String,
    kind: &'static str,
    status: &'static str,
    error: Option<String>,
    cancel: runtime::Cancellation,
}
impl AccelerationState {
    pub fn busy(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.stopping
            || inner.cleanup_unconfirmed
            || inner
                .operation
                .as_ref()
                .is_some_and(|o| o.status == "running")
    }
    pub fn begin_shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stopping = true;
        inner.selection = None;
        inner.prepared = None;
        if let Some(operation) = &inner.operation {
            operation.cancel.cancel();
        }
    }
    pub(crate) fn shutdown_result(&self) -> Result<(), String> {
        if self.inner.lock().unwrap().cleanup_unconfirmed {
            Err("The selected runtime check's cleanup could not be confirmed. Local setup remains blocked.".into())
        } else {
            Ok(())
        }
    }
    fn begin(&self, kind: &'static str) -> ApiResult<(String, runtime::Cancellation)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.stopping
            || inner.cleanup_unconfirmed
            || inner
                .operation
                .as_ref()
                .is_some_and(|o| o.status == "running")
        {
            return Err(setup_busy());
        }
        let id = crate::id();
        let cancel = runtime::Cancellation::new();
        inner.operation = Some(Operation {
            id: id.clone(),
            kind,
            status: "running",
            error: None,
            cancel: cancel.clone(),
        });
        Ok((id, cancel))
    }
    fn finish(&self, id: &str, result: ApiResult<()>) {
        let mut inner = self.inner.lock().unwrap();
        let cleanup_unconfirmed = inner.cleanup_unconfirmed;
        if let Some(operation) = inner
            .operation
            .as_mut()
            .filter(|o| o.id == id && o.status == "running")
        {
            operation.status = if cleanup_unconfirmed {
                "failed"
            } else if operation.cancel.is_cancelled() {
                "cancelled"
            } else if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            };
            operation.error = result.err().map(|e| e.1);
        }
    }
}
fn setup_busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Finish or cancel local model setup before changing its settings.".into(),
    )
}
fn preference_error(error: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::CONFLICT, error.to_string())
}
pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/acceleration", get(status))
        .route("/api/acceleration/inventory", post(inventory))
        .route(
            "/api/acceleration/preferences",
            axum::routing::put(preferences),
        )
        .route("/api/acceleration/preferences/reset", post(reset))
        .route("/api/acceleration/preferences/rollback", post(rollback))
        .route("/api/acceleration/cancel", post(cancel))
        .route("/api/acceleration/inspect", post(inspect))
        .route("/api/acceleration/plan", post(prepare))
        .route("/api/acceleration/apply", post(apply))
}
async fn status(State(a): State<Arc<App>>) -> ApiResult<Json<Value>> {
    let preferences = runtime_preferences::read(&a.db.lock().unwrap()).map_err(preference_error)?;
    let inner = a.acceleration.inner.lock().unwrap();
    Ok(Json(json!({
        "preferences": preferences,
        "inventory": inner.inventory,
        "selection": inner.selection.as_ref().map(|s|json!({"id":s.id,"model_id":s.model_id,"files":s.files,"catalog":s.catalog.as_ref().map(|b|b.summary()),"probe":s.probe.as_ref().map(|p|p.summary())})),
        "preview": inner.preview,
        "prepared": inner.prepared.as_ref().map(|p|json!({"plan":p.plan.summary(),"settings_revision":p.preferences.revision,"output_tokens":p.preferences.record.output_tokens,"qualification":"unvalidated","memory_fit":"unknown","catalog":p.catalog.as_ref().map(|b|b.summary())})),
        "operation": inner.operation.as_ref().map(|o| json!({"id":o.id,"kind":o.kind,"status":o.status,"error":o.error})),
        "automatic_qualification": "requires_qualified_runtime_and_model",
        "execution_evidence": "not_observed",
        "cleanup_unconfirmed": inner.cleanup_unconfirmed
    })))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreferencesInput {
    expected_revision: u64,
    intent: planning::OptimizationIntent,
}
async fn preferences(
    State(a): State<Arc<App>>,
    Json(input): Json<PreferencesInput>,
) -> ApiResult<Json<Value>> {
    input.intent.validate().map_err(|e| bad(&e.to_string()))?;
    let _consent = a.consent.lock().unwrap();
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(setup_busy());
    }
    let saved =
        runtime_preferences::update(&a.db.lock().unwrap(), input.expected_revision, input.intent)
            .map_err(preference_error)?;
    invalidate_advice(&a);
    Ok(Json(json!({"preferences": saved})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionInput {
    expected_revision: u64,
}
async fn reset(
    State(a): State<Arc<App>>,
    Json(input): Json<RevisionInput>,
) -> ApiResult<Json<Value>> {
    let _consent = a.consent.lock().unwrap();
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(setup_busy());
    }
    let saved =
        runtime_preferences::reset_overrides(&a.db.lock().unwrap(), input.expected_revision)
            .map_err(preference_error)?;
    invalidate_advice(&a);
    Ok(Json(json!({"preferences":saved})))
}
async fn rollback(
    State(a): State<Arc<App>>,
    Json(input): Json<RevisionInput>,
) -> ApiResult<Json<Value>> {
    let _consent = a.consent.lock().unwrap();
    if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
        return Err(setup_busy());
    }
    let saved =
        runtime_preferences::rollback_intent(&a.db.lock().unwrap(), input.expected_revision)
            .map_err(preference_error)?;
    invalidate_advice(&a);
    Ok(Json(json!({"preferences":saved,"runtime_restarted":false})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryInput {
    expected_revision: u64,
}
fn invalidate_advice(a: &App) {
    let mut inner = a.acceleration.inner.lock().unwrap();
    inner.prepared = None;
    inner.preview = None;
}
async fn inventory(
    State(a): State<Arc<App>>,
    Json(input): Json<InventoryInput>,
) -> ApiResult<Json<Value>> {
    let (operation, cancel) = {
        let _consent = a.consent.lock().unwrap();
        let preferences =
            runtime_preferences::read(&a.db.lock().unwrap()).map_err(preference_error)?;
        if preferences.revision != input.expected_revision {
            return Err(stale_preferences());
        }
        if a.native.busy() || a.mlx.busy() {
            return Err(setup_busy());
        }
        a.acceleration.begin("inventory")?
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = async {
            let observation = tokio::task::spawn_blocking(hardware_inventory::observe)
                .await
                .map_err(|_| bad("The native inventory worker failed."))?
                .map_err(|e| bad(&e.to_string()))?;
            let _consent = a.consent.lock().unwrap();
            if cancel.is_cancelled() {
                return Err(bad("Hardware query cancelled."));
            }
            if runtime_preferences::read(&a.db.lock().unwrap())
                .map_err(preference_error)?
                .revision
                != input.expected_revision
            {
                return Err(stale_preferences());
            }
            let mut inner = a.acceleration.inner.lock().unwrap();
            if inner.stopping {
                return Err(setup_busy());
            }
            inner.inventory = Some(observation);
            inner.prepared = None;
            inner.preview = None;
            // Cancellation and committing the observation share the same lock.
            if let Some(op) = inner.operation.as_mut().filter(|o| o.id == operation) {
                op.status = "succeeded";
            }
            Ok(())
        }
        .await;
        a.acceleration.finish(&operation, result);
    });
    Ok(Json(response))
}
fn stale_preferences() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Local inference settings changed. Refresh and review the current settings.".into(),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    operation_id: String,
}
async fn cancel(
    State(a): State<Arc<App>>,
    Json(input): Json<CancelInput>,
) -> ApiResult<Json<Value>> {
    let _consent = a.consent.lock().unwrap();
    let inner = a.acceleration.inner.lock().unwrap();
    let operation = inner
        .operation
        .as_ref()
        .filter(|o| o.id == input.operation_id && o.status == "running")
        .ok_or_else(|| bad("That setup operation is no longer running."))?;
    operation.cancel.cancel();
    Ok(Json(
        json!({"ok":true,"message":"Waiting for owned setup work to stop."}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectInput {
    executable: runtime_catalog::SelectedFileInput,
    model: runtime_catalog::SelectedFileInput,
    model_id: String,
    expected_revision: u64,
    #[serde(flatten)]
    expected: Expected,
}
fn check_settings(a: &App, revision: u64) -> ApiResult<runtime_preferences::Preferences> {
    let saved = runtime_preferences::read(&a.db.lock().unwrap()).map_err(preference_error)?;
    if saved.revision != revision {
        return Err(stale_preferences());
    }
    Ok(saved)
}
async fn inspect(
    State(a): State<Arc<App>>,
    Json(input): Json<InspectInput>,
) -> ApiResult<Json<Value>> {
    inspect_selected(a, input, None).await
}
pub(crate) async fn inspect_catalog(
    a: Arc<App>,
    selected: Arc<local_catalog_api::PreparedCatalog>,
    model_id: String,
    expected_revision: u64,
    expected: Expected,
) -> ApiResult<Json<Value>> {
    let input = InspectInput {
        executable: selected.selected_file(true, smith_local_catalog::FileRole::Executable)?,
        model: selected.selected_file(false, smith_local_catalog::FileRole::Weights)?,
        model_id,
        expected_revision,
        expected,
    };
    if selected.entry().manifest.description.format != smith_local_catalog::ModelFormat::Gguf {
        return Err(bad("Choose the setup adapter matching this catalog entry."));
    }
    inspect_selected(a, input, Some(selected)).await
}
async fn inspect_selected(
    a: Arc<App>,
    input: InspectInput,
    catalog: Option<Arc<local_catalog_api::PreparedCatalog>>,
) -> ApiResult<Json<Value>> {
    if input.model_id.is_empty()
        || input.model_id.len() > 160
        || !input
            .model_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(bad(
            "Use a local model name with letters, numbers, dot, underscore or hyphen.",
        ));
    }
    let (operation, cancel) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        check_settings(&a, input.expected_revision)?;
        if let Some(selected) = &catalog {
            local_catalog_api::validate_binding_locked(&a, &consent, &selected.binding())?;
        }
        if a.native.busy() || a.mlx.busy() {
            return Err(setup_busy());
        }
        let started = a.acceleration.begin("inspect")?;
        let mut inner = a.acceleration.inner.lock().unwrap();
        inner.selection = None;
        inner.prepared = None;
        inner.preview = None;
        started
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = async {
            let files = runtime_catalog::inspect_selection(input.executable, input.model, &cancel)
                .await
                .map_err(|e| bad(&format!("Selected files could not be inspected: {e}")))?;
            if let Some(selected) = &catalog {
                selected.require_gguf(&files)?;
            }
            let probe = runtime::RuntimeProbePlan::prepare(files.executable.clone(), &cancel)
                .await
                .map_err(native_api::failure)?;
            if let Some(selected) = &catalog {
                selected
                    .binding()
                    .require_gguf_artifacts(&probe.summary().executable, &files.model)?;
            }
            let consent = a.consent.lock().unwrap();
            validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
            check_settings(&a, input.expected_revision)?;
            if let Some(selected) = &catalog {
                local_catalog_api::validate_binding_locked(&a, &consent, &selected.binding())?;
            }
            if cancel.is_cancelled() {
                return Err(bad("Selected-file inspection cancelled."));
            }
            let mut inner = a.acceleration.inner.lock().unwrap();
            if inner.stopping {
                return Err(setup_busy());
            }
            inner.selection = Some(Selection {
                id: crate::id(),
                model_id: input.model_id,
                files,
                expected: input.expected,
                probe: Some(probe),
                catalog: catalog.as_ref().map(|p| p.binding()),
            });
            if let Some(op) = inner.operation.as_mut().filter(|o| o.id == operation) {
                op.status = "succeeded";
            }
            Ok(())
        }
        .await;
        a.acceleration.finish(&operation, result);
    });
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanInput {
    selection_id: String,
    probe_id: String,
    expected_revision: u64,
    confirm_probe: bool,
    #[serde(flatten)]
    expected: Expected,
}
async fn prepare(
    State(a): State<Arc<App>>,
    Json(input): Json<PlanInput>,
) -> ApiResult<Json<Value>> {
    if !input.confirm_probe {
        return Err(bad(
            "Review the selected runtime before running its bounded setup checks.",
        ));
    }
    let (operation, cancel, selected, model_id, probe, preferences, runtime_revision, catalog) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        let preferences = check_settings(&a, input.expected_revision)?;
        if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
            return Err(setup_busy());
        }
        let catalog = a
            .acceleration
            .inner
            .lock()
            .unwrap()
            .selection
            .as_ref()
            .and_then(|s| s.catalog.clone());
        if let Some(binding) = &catalog {
            local_catalog_api::validate_binding_locked(&a, &consent, binding)?;
        }
        let mut inner = a.acceleration.inner.lock().unwrap();
        let selection = inner
            .selection
            .as_mut()
            .filter(|s| {
                s.id == input.selection_id && s.expected.revision == input.expected.revision
            })
            .ok_or_else(|| bad("Inspect the selected files again before preparing a new plan."))?;
        let probe = selection
            .probe
            .as_ref()
            .filter(|p| p.summary().id == input.probe_id)
            .ok_or_else(|| bad("The reviewed runtime check changed or was already used."))?;
        let exact_id = probe.summary().id.clone();
        let approved = selection
            .probe
            .take()
            .unwrap()
            .approve(&exact_id)
            .map_err(native_api::failure)?;
        let files = selection.files.clone();
        let model_id = selection.model_id.clone();
        inner.prepared = None;
        inner.preview = None;
        drop(inner);
        let (operation, cancel) = a.acceleration.begin("plan")?;
        (
            operation,
            cancel,
            files,
            model_id,
            approved,
            preferences,
            a.native.revision(),
            catalog,
        )
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = async {
            let mut probe = runtime::RuntimeProbe::start(probe, cancel.clone());
            let report = match probe.wait().await {
                Ok(report) => report,
                Err(runtime::Failure::Cleanup) => {
                    a.acceleration.inner.lock().unwrap().cleanup_unconfirmed = true;
                    return Err(bad("The selected runtime check's cleanup could not be confirmed. Further setup is blocked; no model was loaded."));
                }
                Err(error) => return Err(native_api::failure(error)),
            };
            if cancel.is_cancelled() {
                return Err(bad("Runtime check cancelled."));
            }
            let inventory = tokio::task::spawn_blocking(hardware_inventory::observe)
                .await
                .map_err(|_| bad("Native inventory failed."))?
                .map_err(|e| bad(&e.to_string()))?;
            let advice = setup_advice(&inventory, &selected, &model_id, &report, &preferences)?;
            let manifest = runtime::Manifest {
                model_id,
                executable: selected.executable.clone(),
                model: selected.model.clone(),
                protocol: runtime::Protocol::LlamaCppUnixHttpV1,
                execution_kind: runtime::ExecutionKind::Native,
                declares_text_chat: true,
            };
            let plan = runtime::LoadPlan::prepare_with_options(
                manifest,
                advice.config,
                advice.advanced,
                &report.compatibility,
                &cancel,
            )
            .await
            .map_err(native_api::failure)?;
            if let Some(binding)=&catalog {binding.require_gguf_artifacts(&plan.summary().manifest.executable,&plan.summary().manifest.model)?;}
            let consent = a.consent.lock().unwrap();
            validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
            check_settings(&a, input.expected_revision)?;
            if cancel.is_cancelled() {
                return Err(bad("Setup plan cancelled."));
            }
            let catalog=catalog.as_ref().map(|b|b.with_inventory_locked(&a,&consent,inventory.inventory.generation.clone())).transpose()?;
            let mut inner = a.acceleration.inner.lock().unwrap();
            if inner.stopping
                || inner
                    .selection
                    .as_ref()
                    .is_none_or(|s| s.id != input.selection_id)
            {
                return Err(setup_busy());
            }
            if let Some(selection)=inner.selection.as_mut() {selection.catalog=catalog.clone();}
            inner.preview = Some(advice.review);
            inner.inventory = Some(inventory.clone());
            inner.prepared = Some(PreparedSetup {
                selection_id: input.selection_id,
                runtime_revision,
                plan,
                expected: input.expected,
                preferences: native_api::PreparedPreferences {
                    revision: preferences.revision,
                    intent: preferences.intent,
                    record: advice.record,
                },
                inventory,
                device_id: advice.device_id,
                catalog,
            });
            if let Some(op) = inner.operation.as_mut().filter(|o| o.id == operation) {
                op.status = "succeeded";
            }
            Ok(())
        }
        .await;
        a.acceleration.finish(&operation, result);
    });
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyInput {
    plan_id: String,
    #[serde(default)]
    catalog_review_id: Option<String>,
    expected_revision: u64,
    confirm: bool,
    #[serde(flatten)]
    expected: Expected,
}
async fn apply(State(a): State<Arc<App>>, Json(input): Json<ApplyInput>) -> ApiResult<Json<Value>> {
    if !input.confirm {
        return Err(bad(
            "Review the selected local model and settings before loading.",
        ));
    }
    let (prepared, operation, cancel) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        check_settings(&a, input.expected_revision)?;
        if a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
            return Err(setup_busy());
        }
        let catalog = a
            .acceleration
            .inner
            .lock()
            .unwrap()
            .prepared
            .as_ref()
            .and_then(|p| p.catalog.clone());
        if let Some(binding) = &catalog {
            local_catalog_api::validate_binding_locked(&a, &consent, binding)?;
        }
        let mut inner = a.acceleration.inner.lock().unwrap();
        let prepared = inner
            .prepared
            .as_ref()
            .filter(|p| {
                p.plan.summary().id == input.plan_id
                    && p.preferences.revision == input.expected_revision
                    && p.expected.revision == input.expected.revision
            })
            .ok_or_else(|| bad("This setup plan changed or was already used. Prepare it again."))?;
        if prepared.catalog.as_ref().map(|b| b.summary().id.as_str())
            != input.catalog_review_id.as_deref()
        {
            return Err(bad("Review the exact catalog-bound setup before applying. Its catalog review changed or is missing."));
        }
        if prepared.inventory.observed_unix_ms.saturating_add(300_000) < now().saturating_mul(1000)
        {
            return Err(bad(
                "Hardware observation expired. Prepare a fresh setup plan.",
            ));
        }
        let prepared = inner.prepared.take().unwrap();
        drop(inner);
        let (operation, cancel) = a.acceleration.begin("apply")?;
        (prepared, operation, cancel)
    };
    apply_selected(a, prepared, operation, cancel, async {
        tokio::task::spawn_blocking(hardware_inventory::observe)
            .await
            .map_err(|_| bad("Native inventory failed."))?
            .map_err(|e| bad(&e.to_string()))
    })
    .await
}
// The observation is an owned future, separate from request lifetime. Keeping
// this boundary explicit also permits deterministic cancellation/race tests.
async fn apply_selected<F>(
    a: Arc<App>,
    prepared: PreparedSetup,
    operation: String,
    cancel: runtime::Cancellation,
    observation: F,
) -> ApiResult<Json<Value>>
where
    F: std::future::Future<Output = ApiResult<hardware_inventory::Observation>> + Send + 'static,
{
    let (reply, response) = tokio::sync::oneshot::channel();
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = async {
            let current = observation.await?;
            let selected = prepared
                .inventory
                .inventory
                .devices
                .iter()
                .find(|d| d.id == prepared.device_id);
            if current.inventory.os != prepared.inventory.inventory.os
                || current.inventory.architecture != prepared.inventory.inventory.architecture
                || current.inventory.os_version != prepared.inventory.inventory.os_version
                || selected.is_none()
                || current
                    .inventory
                    .devices
                    .iter()
                    .find(|d| d.id == prepared.device_id)
                    != selected
            {
                return Err(bad(
                    "The selected hardware changed. Prepare a fresh setup plan.",
                ));
            }
            // The actual native load rehashes the selected files and checks exact owned
            // readiness. The preview is not used as an execution or hardware certificate.
            let consent = a.consent.lock().unwrap();
            {
                let inner = a.acceleration.inner.lock().unwrap();
                if cancel.is_cancelled()
                    || inner.stopping
                    || inner
                        .operation
                        .as_ref()
                        .is_none_or(|o| o.id != operation || o.status != "running")
                    || inner
                        .selection
                        .as_ref()
                        .is_none_or(|s| s.id != prepared.selection_id)
                {
                    return Err(bad(
                        "Local model setup was cancelled or replaced before loading.",
                    ));
                }
            }
            if let Some(binding) = &prepared.catalog {
                local_catalog_api::validate_binding_locked(&a, &consent, binding)?;
            }
            let response = native_api::admit_selected_plan(
                &a,
                &consent,
                prepared.runtime_revision,
                prepared.expected,
                prepared.plan,
                prepared.preferences,
                prepared.catalog,
            )?;
            // Transfer and cancellation share the consent lock: after this point the
            // native operation owns cleanup, and setup cancel cannot claim success.
            a.acceleration.finish(&operation, Ok(()));
            Ok(response)
        }
        .await;
        if let Err(error) = &result {
            a.acceleration
                .finish(&operation, Err(ApiError(error.0, error.1.clone())));
        }
        let _ = reply.send(result);
    });
    response
        .await
        .map_err(|_| bad("The owned setup operation did not return a result."))?
}

struct SetupAdvice {
    config: runtime::LoadConfig,
    advanced: runtime::AdvancedSettings,
    record: runtime_preferences::SelectedSuccessRecord,
    device_id: String,
    review: Value,
}

fn setup_advice(
    observation: &hardware_inventory::Observation,
    selected: &runtime_catalog::CatalogSelection,
    model_id: &str,
    probe: &runtime::RuntimeProbeReport,
    preferences: &runtime_preferences::Preferences,
) -> ApiResult<SetupAdvice> {
    use planning::{Backend, DeviceClass, KvType, PreviewOutcome};
    if !probe.compatibility.protocol_candidate() {
        return Err(bad("This executable does not report the fixed offline Unix-socket protocol required by Smith. Its setup checks did not load a model."));
    }
    let inventory = &observation.inventory;
    let intent = &preferences.intent;
    let o = &intent.overrides;
    if intent.capability != planning::Capability::TextChat {
        return Err(bad("This selected GGUF runtime setup currently supports text chat. Other capability routes remain separate."));
    }
    if o.host_budget_bytes.is_some()
        || o.device_budget_bytes.is_some()
        || o.headroom_bytes.is_some()
    {
        return Err(bad("This unqualified runtime cannot enforce a memory budget yet. The saved overrides are retained; load was not started."));
    }
    let matched: Vec<_> = probe
        .devices
        .iter()
        .filter(|d| d.backend == runtime::DeviceBackend::Metal)
        .filter_map(|runtime_device| {
            let described = runtime_device
                .description
                .split('(')
                .next()
                .unwrap_or("")
                .trim();
            let candidates: Vec<_> = observation
                .metal_devices
                .iter()
                .filter(|d| {
                    d.name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(described))
                })
                .collect();
            if candidates.len() != 1 {
                return None;
            }
            inventory
                .devices
                .iter()
                .find(|d| d.id == candidates[0].device_id)
                .map(|d| (d, runtime_device))
        })
        .collect();
    let cpu = inventory
        .devices
        .iter()
        .find(|d| d.class == DeviceClass::Cpu);
    let pinned_metal: Vec<_> = matched
        .iter()
        .filter(|(d, _)| o.device_id.as_deref().is_none_or(|id| id == d.id))
        .collect();
    let (backend,device,runtime_device)=match o.backend {
        Some(Backend::LlamaCpu) => (Backend::LlamaCpu,inventory.devices.iter().find(|d|d.class==DeviceClass::Cpu).ok_or_else(||bad("Native CPU inventory is unavailable."))?,None),
        Some(Backend::LlamaMetal) => {
            if pinned_metal.len()!=1 { return Err(bad("The pinned Metal device cannot be matched uniquely to this runtime and current hardware.")); }
            (Backend::LlamaMetal,pinned_metal[0].0,Some(pinned_metal[0].1))
        },
        Some(_) => return Err(bad("The pinned backend is not supplied by this selected Mac GGUF runtime. Its override was not replaced.")),
        None if o.device_id.is_some() => {
            if let Some(cpu) = cpu.filter(|d|Some(d.id.as_str())==o.device_id.as_deref()) {
                (Backend::LlamaCpu,cpu,None)
            } else if pinned_metal.len()==1 {
                (Backend::LlamaMetal,pinned_metal[0].0,Some(pinned_metal[0].1))
            } else { return Err(bad("The pinned device cannot be matched uniquely to a compatible selected runtime backend.")); }
        },
        None if matched.len()==1 && intent.preference!=planning::Preference::Quiet => (Backend::LlamaMetal,matched[0].0,Some(matched[0].1)),
        None => (Backend::LlamaCpu,inventory.devices.iter().find(|d|d.class==DeviceClass::Cpu).ok_or_else(||bad("Native CPU inventory is unavailable."))?,None),
    };
    if o.device_id.as_deref().is_some_and(|id| id != device.id) {
        return Err(bad(
            "The pinned device is not available for the selected runtime; it was not replaced.",
        ));
    }
    let context=selected.metadata.context_tokens.filter(|v|(128..=1_048_576).contains(v)).ok_or_else(||bad("This GGUF does not provide a supported context limit for setup. Use a supported complete model."))?;
    let architecture = selected
        .metadata
        .architecture
        .clone()
        .ok_or_else(|| bad("This GGUF does not declare its model architecture."))?;
    let layers = if backend == Backend::LlamaCpu {
        0
    } else {
        o.gpu_layers
            .or(selected.metadata.layers)
            .filter(|v| *v > 0 && *v <= 10000)
            .ok_or_else(|| {
                bad("Metal setup needs a known layer count or an explicit valid override.")
            })?
    };
    let compat = &probe.compatibility;
    for (pinned, supported) in [
        (
            o.prefill_threads.is_some(),
            compat.supports_flag("--threads-batch"),
        ),
        (
            o.batch_tokens.is_some(),
            compat.supports_flag("--batch-size"),
        ),
        (
            o.microbatch_tokens.is_some(),
            compat.supports_flag("--batch-size") && compat.supports_flag("--ubatch-size"),
        ),
        (
            o.flash_attention.is_some(),
            compat.supports_flash_attention(),
        ),
        (
            o.mmap.is_some() || o.mlock.is_some(),
            compat.supports_memory_settings(),
        ),
    ] {
        if pinned && !supported {
            return Err(bad("A pinned advanced setting is not reported by this runtime. Its override was not ignored."));
        }
    }
    let kv = o.kv_type.unwrap_or(KvType::F16);
    let runtime_kv = match kv {
        KvType::F16 => runtime::KvType::F16,
        KvType::Q8 => runtime::KvType::Q8_0,
        KvType::Q4 => runtime::KvType::Q4_0,
        _ => {
            return Err(bad(
                "The selected runtime adapter does not support that pinned KV type.",
            ))
        }
    };
    if !compat.supports_kv_type(runtime_kv) {
        return Err(bad(
            "The selected runtime does not report the requested KV cache type.",
        ));
    }
    let model = planning::ModelDescriptor {
        id: model_id.into(),
        bundle_sha256: selected.model.sha256.clone(),
        format: planning::ModelFormat::GgufBundle,
        architecture,
        quantization: "exact_selected_gguf".into(),
        capabilities: vec![planning::Capability::TextChat],
        native_context_limit: context,
    };
    let adapter = planning::AdapterDescriptor {
        id: format!("selected-{:?}", backend).to_lowercase(),
        backend,
        os: inventory.os,
        architecture: inventory.architecture,
        version: "selected-executable-cli-observation".into(),
        bundle_sha256: None,
        selected_executable_sha256: Some(selected.executable.sha256.clone()),
        availability: planning::Availability::InstalledUnverified,
        capabilities: vec![planning::Capability::TextChat],
        qualified_devices: vec![],
        settings: planning::SettingsLimits {
            max_context_tokens: context,
            max_output_tokens: context,
            max_threads: inventory.logical_cpu_count.min(512),
            max_batch_tokens: if compat.supports_flag("--batch-size") {
                32768
            } else {
                1
            },
            max_microbatch_tokens: if compat.supports_flag("--ubatch-size") {
                32768
            } else {
                1
            },
            supports_flash_attention: compat.supports_flash_attention(),
            supports_mmap: compat.supports_memory_settings(),
            supports_mlock: compat.supports_memory_settings(),
        },
        models: vec![planning::ModelProfile {
            model_bundle_sha256: model.bundle_sha256.clone(),
            format: model.format,
            architecture: model.architecture.clone(),
            quantization: model.quantization.clone(),
            kv_type: kv,
            gpu_layers: Some(layers),
            memory: None,
        }],
    };
    let mut preview_intent = intent.clone();
    // This narrows derived advice to the actually matched OS/runtime device;
    // it never overwrites the user's stored Auto intent or pinned overrides.
    preview_intent.overrides.device_id = Some(device.id.clone());
    let advice = planning::preview(
        inventory,
        &adapter,
        &model,
        &preview_intent,
        preferences.revision,
    )
    .map_err(|e| bad(&e.to_string()))?;
    let preview = match &advice {
        PreviewOutcome::Preview { preview, .. } => preview,
        PreviewOutcome::Blocked { reasons, .. } => {
            return Err(bad(&format!(
                "Settings cannot be prepared: {}",
                reasons
                    .iter()
                    .map(|r| r.message.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )))
        }
    };
    let s = &preview.settings;
    let advanced = runtime::AdvancedSettings {
        prefill_threads: compat
            .supports_flag("--threads-batch")
            .then_some(s.prefill_threads),
        batch_size: compat
            .supports_flag("--batch-size")
            .then_some(s.batch_tokens),
        micro_batch_size: (compat.supports_flag("--batch-size")
            && compat.supports_flag("--ubatch-size"))
        .then_some(s.microbatch_tokens),
        flash_attention: compat
            .supports_flash_attention()
            .then_some(if s.flash_attention {
                runtime::FlashAttention::On
            } else {
                runtime::FlashAttention::Off
            }),
        cache_type_k: Some(runtime_kv),
        cache_type_v: Some(runtime_kv),
        mmap: compat.supports_memory_settings().then_some(s.mmap),
        mlock: compat.supports_memory_settings().then_some(s.mlock),
    };
    let requested_device = runtime_device.map(|d| d.name.clone());
    let config = runtime::LoadConfig {
        backend: match &requested_device {
            Some(name) => runtime::BackendRequest::Device {
                name: name.clone(),
                gpu_layers: layers,
            },
            None => runtime::BackendRequest::Cpu,
        },
        context_tokens: s.context_tokens,
        threads: s.threads,
        budgets: runtime::Budgets {
            startup: Duration::from_secs(90),
            generation: Duration::from_secs(60),
            reap: Duration::from_secs(2),
        },
    };
    let record = runtime_preferences::SelectedSuccessRecord {
        model_sha256: selected.model.sha256.clone(),
        executable_sha256: selected.executable.sha256.clone(),
        requested_backend: if requested_device.is_some() {
            runtime_preferences::SelectedBackend::Device
        } else {
            runtime_preferences::SelectedBackend::Cpu
        },
        requested_device,
        context_tokens: s.context_tokens,
        output_tokens: s.output_tokens,
        threads: s.threads,
        gpu_layers: Some(layers),
        evidence: runtime_preferences::SelectedEvidence::RuntimeReadyPlacementUnknown,
    };
    let review = json!({"settings_preview":advice,"stored_intent":intent,"applied_config":config,"applied_advanced":advanced,"runtime_check":{"id":probe.probe_id,"devices":probe.devices,"compatibility":probe.compatibility,"inference_verified":false},"memory_fit":"unknown","placement":"unverified","note":"Recommended starting settings from metadata and native inventory. Load is explicit; memory fit and accelerated execution have not been measured. Missing advanced flags retain the runtime's defaults and are absent from applied_advanced."});
    Ok(SetupAdvice {
        config,
        advanced,
        record,
        device_id: device.id.clone(),
        review,
    })
}

/// Cached observation only. Listing the passive catalog never invokes this.
pub(crate) fn catalog_inventory_generation(a: &App) -> ApiResult<String> {
    let inner = a.acceleration.inner.lock().unwrap();
    let observation = inner
        .inventory
        .as_ref()
        .filter(|o| o.observed_unix_ms.saturating_add(300_000) >= now().saturating_mul(1000))
        .ok_or_else(|| {
            bad("Observe hardware explicitly before reviewing a saved local selection.")
        })?;
    if inner.stopping {
        return Err(setup_busy());
    }
    Ok(observation.inventory.generation.clone())
}

#[cfg(test)]
pub(crate) fn set_catalog_inventory_for_test(
    a: &App,
    observation: hardware_inventory::Observation,
) {
    a.acceleration.inner.lock().unwrap().inventory = Some(observation);
}

#[cfg(test)]
pub(crate) fn stage_catalog_apply_for_test(
    a: &App,
    plan: runtime::LoadPlan,
    binding: Option<local_catalog_api::CatalogBinding>,
) {
    let preferences = runtime_preferences::read(&a.db.lock().unwrap()).unwrap();
    let expected = {
        let consent = a.consent.lock().unwrap();
        let c = a.config.lock().unwrap();
        Expected {
            revision: consent.token(),
            provider: c.provider.clone(),
            endpoint: c.endpoint.clone(),
        }
    };
    let record = runtime_preferences::SelectedSuccessRecord {
        model_sha256: plan.summary().manifest.model.sha256.clone(),
        executable_sha256: plan.summary().manifest.executable.sha256.clone(),
        requested_backend: runtime_preferences::SelectedBackend::Cpu,
        requested_device: None,
        context_tokens: 4096,
        output_tokens: 128,
        threads: 1,
        gpu_layers: None,
        evidence: runtime_preferences::SelectedEvidence::RuntimeReadyPlacementUnknown,
    };
    let mut inner = a.acceleration.inner.lock().unwrap();
    let inventory = inner.inventory.as_ref().unwrap().clone();
    inner.prepared = Some(PreparedSetup {
        selection_id: "synthetic-not-dispatched".into(),
        runtime_revision: a.native.revision(),
        plan,
        expected,
        preferences: native_api::PreparedPreferences {
            revision: preferences.revision,
            intent: preferences.intent,
            record,
        },
        inventory,
        device_id: "synthetic-not-dispatched".into(),
        catalog: binding,
    });
}

#[cfg(test)]
pub(crate) fn manual_catalog_review_absent_for_test(input: Value) -> bool {
    serde_json::from_value::<ApplyInput>(input).is_ok_and(|v| v.catalog_review_id.is_none())
}
