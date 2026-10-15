//! Explicit local MLX worker custody. A stored success record is descriptive;
//! only a freshly approved, live actor can admit generation.
use crate::inference_provenance::{ModelProvenance, ProvenanceSampling, Provider};
use crate::*;
use sha2::{Digest, Sha256};
use smith_mlx_runtime as mlx;
use std::sync::atomic::{AtomicBool, Ordering};

const SETTINGS_KEY: &str = "mlx_settings";
const MAX_REVISION: u64 = 9_007_199_254_740_991;
#[derive(Default)]
pub struct MlxState {
    inner: Mutex<Inner>,
    generation: Arc<AtomicBool>,
    commit_gate: Mutex<()>,
}
#[derive(Default)]
struct Inner {
    runtime: Option<mlx::Runtime>,
    loaded: Option<Loaded>,
    prepared: Option<Prepared>,
    operation: Option<HostOperation>,
    stopping: bool,
    cleanup_unconfirmed: bool,
}
#[derive(Clone)]
struct Loaded {
    model_id: String,
    endpoint: String,
    capability: mlx::Capability,
    sampling: Sampling,
    thinking_mode: mlx::ThinkingMode,
    review_id: String,
    provenance: Option<ModelProvenance>,
}
struct Prepared {
    plan: mlx::LaunchPlan,
    expected: Expected,
    settings_revision: u64,
    loaded: Loaded,
    catalog: Option<local_catalog_api::CatalogBinding>,
}
struct HostOperation {
    id: String,
    kind: &'static str,
    status: &'static str,
    error: Option<String>,
    config_revision: Option<String>,
    cancel: mlx::Cancellation,
    cancel_requested: bool,
    progress: Option<mlx::Progress>,
    completion: Option<mlx::Completion>,
}
impl MlxState {
    #[cfg(test)]
    pub(crate) fn inject_cleanup_fault_for_test(&self) {
        self.inner.lock().unwrap().cleanup_unconfirmed = true;
    }
    pub fn busy(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.stopping
            || inner.cleanup_unconfirmed
            || running(&inner)
            || self.generation.load(Ordering::Acquire)
            || inner.runtime.as_ref().is_some_and(|r| {
                let s = r.status();
                !matches!(
                    s.phase,
                    mlx::Phase::Ready | mlx::Phase::Idle | mlx::Phase::Stopped | mlx::Phase::Failed
                ) || s.failure == Some(mlx::Failure::Cleanup)
                    || (s.phase == mlx::Phase::Failed && s.spawned != s.reaped)
            })
    }
    pub fn occupied(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.cleanup_unconfirmed
            || inner.runtime.as_ref().is_some_and(|r| {
                let s = r.status();
                s.pid.is_some() || s.spawned != s.reaped || s.failure == Some(mlx::Failure::Cleanup)
            })
    }
    pub fn begin_shutdown(&self) {
        let _commit = self.commit_gate.lock().unwrap();
        let mut inner = self.inner.lock().unwrap();
        inner.stopping = true;
        inner.prepared = None;
        // A successful load's token belongs to the live actor. Do not cancel it
        // here: profile shutdown first cancels/drains chats and persists output.
        if let Some(op) = inner.operation.as_mut().filter(|o| o.status == "running") {
            op.cancel_requested = true;
            op.cancel.cancel();
        }
    }
    pub async fn shutdown(&self) -> Result<(), String> {
        self.begin_shutdown();
        let runtime = self.inner.lock().unwrap().runtime.clone();
        if let Some(runtime) = runtime {
            cleanup(self, &runtime).await.map_err(|e| e.1)?;
        }
        if self.inner.lock().unwrap().cleanup_unconfirmed {
            return Err(failure(mlx::Failure::Cleanup).1);
        }
        Ok(())
    }
    fn begin(&self, kind: &'static str) -> ApiResult<(String, mlx::Cancellation)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.stopping
            || inner.cleanup_unconfirmed
            || running(&inner)
            || self.generation.load(Ordering::Acquire)
        {
            return Err(busy());
        }
        let id = crate::id();
        let cancel = mlx::Cancellation::new();
        inner.operation = Some(HostOperation {
            id: id.clone(),
            kind,
            status: "running",
            error: None,
            config_revision: None,
            cancel: cancel.clone(),
            cancel_requested: false,
            progress: None,
            completion: None,
        });
        Ok((id, cancel))
    }
    fn finish(&self, id: &str, result: ApiResult<()>) {
        let mut inner = self.inner.lock().unwrap();
        let fault = inner.cleanup_unconfirmed;
        if let Some(op) = inner
            .operation
            .as_mut()
            .filter(|o| o.id == id && o.status == "running")
        {
            op.status = if fault {
                "failed"
            } else if result.is_ok() {
                "succeeded"
            } else if op.cancel_requested {
                "cancelled"
            } else {
                "failed"
            };
            op.error = result.err().map(|e| e.1);
        }
    }
}
fn running(inner: &Inner) -> bool {
    inner
        .operation
        .as_ref()
        .is_some_and(|o| o.status == "running")
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Finish or cancel the current local runtime operation before continuing.".into(),
    )
}
pub(crate) fn failure(error: mlx::Failure) -> ApiError {
    let status = if matches!(
        error,
        mlx::Failure::Busy
            | mlx::Failure::StaleRevision
            | mlx::Failure::NotLoaded
            | mlx::Failure::ConsentMismatch
    ) {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    let detail = match error {
        mlx::Failure::Cleanup => "Owned MLX cleanup could not be confirmed. The profile and runtime replacement remain blocked until cleanup can be confirmed.",
        mlx::Failure::NotLoaded => "Explicitly prepare and load a chat-capable MLX model before sending.",
        mlx::Failure::UnsupportedCapability => "The selected model does not admit this capability. Raw-only models cannot accept chat messages.",
        mlx::Failure::SelectionChanged => "Selected local bytes changed. Prepare and review a new plan.",
        mlx::Failure::Cancelled => "MLX operation cancelled. A cancelled generation retires its worker; explicitly load again.",
        mlx::Failure::InvalidRequest => "MLX input or sampling exceeds the supported bounds. No truncation or settings fallback was applied.",
        mlx::Failure::UnsupportedPlatform => "Native MLX execution requires macOS and the selected Metal worker.",
        _ => "The MLX operation failed. Review its state before preparing a new plan.",
    };
    // Worker error text is untrusted; do not forward arbitrary worker strings.
    let code = match error {
        mlx::Failure::Worker { .. } => "worker_rejected".to_owned(),
        other => format!("{other:?}"),
    };
    ApiError(status, format!("{detail} ({code})"))
}
async fn cleanup(state: &MlxState, runtime: &mlx::Runtime) -> ApiResult<()> {
    let result = runtime.shutdown().await;
    let mut inner = state.inner.lock().unwrap();
    match result {
        Ok(s) if s.spawned == s.reaped && s.pid.is_none() => {
            if inner
                .runtime
                .as_ref()
                .is_some_and(|r| r.status().session_id == s.session_id)
            {
                inner.runtime = None;
                inner.loaded = None;
            }
            inner.cleanup_unconfirmed = false;
            Ok(())
        }
        _ => {
            inner.cleanup_unconfirmed = true;
            // Keep a clone even if cleanup was initiated before normal publication.
            inner.runtime = Some(runtime.clone());
            Err(failure(mlx::Failure::Cleanup))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Sampling {
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
    #[serde(default)]
    seed: Option<u64>,
}
impl Default for Sampling {
    fn default() -> Self {
        let s = mlx::Sampling::default();
        Self {
            max_tokens: s.max_tokens,
            temperature: s.temperature,
            top_p: s.top_p,
            seed: s.seed,
        }
    }
}
impl Sampling {
    fn validate(&self) -> ApiResult<()> {
        if !(1..=2048).contains(&self.max_tokens)
            || !self.temperature.is_finite()
            || !(0.0..=2.0).contains(&self.temperature)
            || !self.top_p.is_finite()
            || self.top_p <= 0.0
            || self.top_p > 1.0
        {
            return Err(failure(mlx::Failure::InvalidRequest));
        }
        Ok(())
    }
    fn runtime(&self) -> mlx::Sampling {
        mlx::Sampling {
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSettings {
    format_version: u32,
    revision: u64,
    sampling: Sampling,
    capability: mlx::Capability,
    #[serde(default)]
    thinking_mode: mlx::ThinkingMode,
    last_successful: Option<SuccessRecord>,
}
impl Default for SavedSettings {
    fn default() -> Self {
        Self {
            format_version: 2,
            revision: 0,
            sampling: Sampling::default(),
            capability: mlx::Capability::Chat,
            thinking_mode: mlx::ThinkingMode::ModelDefault,
            last_successful: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessRecord {
    #[serde(default)]
    thinking_mode: mlx::ThinkingMode,
    #[serde(default)]
    review_id: Option<String>,
    model_id: String,
    plan_id: String,
    worker_sha256: String,
    runtime_bundle_sha256: String,
    package_sha256: String,
    capability: mlx::Capability,
    template: Option<mlx::Template>,
    context_limit: u32,
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn settings_error() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Stored MLX settings are malformed or unsupported; they were not reset.".into(),
    )
}
fn read_settings(db: &Connection) -> ApiResult<SavedSettings> {
    let raw: Option<Option<String>> = db.query_row(
        "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))<=65536 THEN value END FROM settings WHERE key=?1",
        [SETTINGS_KEY], |row| row.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(SavedSettings::default());
    };
    let raw = raw.as_deref().ok_or_else(settings_error)?;
    let shape: Value = serde_json::from_str(raw).map_err(|_| settings_error())?;
    let success = shape.get("last_successful").ok_or_else(settings_error)?;
    // Legacy rows predate thinking controls entirely. New rows must retain all
    // requested-intent fields; missing keys must never silently change a mode.
    match shape.get("format_version").and_then(Value::as_u64) {
        Some(1)
            if shape.get("thinking_mode").is_none()
                && success.get("thinking_mode").is_none()
                && success.get("review_id").is_none() => {}
        Some(2)
            if shape.get("thinking_mode").is_some()
                && success.get("thinking_mode").is_some()
                && success.get("review_id").and_then(Value::as_str).is_some() => {}
        _ => return Err(settings_error()),
    }
    let value: SavedSettings = serde_json::from_str(raw).map_err(|_| settings_error())?;
    if !matches!(value.format_version, 1 | 2)
        || value.revision == 0
        || value.revision > MAX_REVISION
        || value.sampling.validate().is_err()
    {
        return Err(settings_error());
    }
    let s = value.last_successful.as_ref().ok_or_else(settings_error)?;
    if !model_id(&s.model_id)
        || !digest(&s.plan_id)
        || !digest(&s.worker_sha256)
        || !digest(&s.runtime_bundle_sha256)
        || !digest(&s.package_sha256)
        || s.capability != value.capability
        || s.thinking_mode != value.thinking_mode
        || s.review_id.as_deref().is_some_and(|r| !digest(r))
        || (s.thinking_mode == mlx::ThinkingMode::Disabled
            && (s.capability != mlx::Capability::Chat
                || s.template != Some(mlx::Template::Qwen3DefaultThinking)
                || s.review_id.is_none()))
        || (s.capability == mlx::Capability::Chat && s.template.is_none())
        || s.context_limit == 0
        || s.context_limit > 10240
        || value.sampling.max_tokens > s.context_limit
    {
        return Err(settings_error());
    }
    Ok(value)
}
fn check_settings(db: &Connection, revision: u64) -> ApiResult<()> {
    if read_settings(db)?.revision != revision {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "MLX settings changed. Prepare and review a new plan.".into(),
        ));
    }
    Ok(())
}
fn save_settings(
    tx: &rusqlite::Transaction<'_>,
    revision: u64,
    loaded: &Loaded,
    record: SuccessRecord,
) -> ApiResult<()> {
    check_settings(tx, revision)?;
    let next = revision
        .checked_add(1)
        .filter(|r| *r <= MAX_REVISION)
        .ok_or_else(settings_error)?;
    let saved = SavedSettings {
        format_version: 2,
        revision: next,
        sampling: loaded.sampling.clone(),
        capability: loaded.capability,
        thinking_mode: loaded.thinking_mode,
        last_successful: Some(record),
    };
    let raw = serde_json::to_string(&saved).map_err(|_| settings_error())?;
    tx.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![SETTINGS_KEY,raw])?;
    Ok(())
}

fn review_intent(
    input: &PrepareInput,
    plan_id: &str,
    catalog: Option<&local_catalog_api::CatalogBinding>,
) -> ApiResult<String> {
    let intent = json!({"format_version":1,"runtime_plan_id":plan_id,"model_id":input.model_id,"capability":input.capability,"sampling":input.sampling,"thinking_mode":input.thinking_mode,"settings_revision":input.settings_revision,"expected":{"revision":input.expected.revision,"provider":input.expected.provider,"endpoint":input.expected.endpoint},"catalog":catalog.map(|b|b.summary())});
    Ok(Sha256::digest(
        serde_json::to_vec(&intent).map_err(|_| failure(mlx::Failure::InvalidRequest))?,
    )
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect())
}
fn loaded_provenance(
    loaded: &Loaded,
    plan: &mlx::PlanSummary,
    manifest: &mlx::Manifest,
    expected_settings: u64,
) -> ApiResult<ModelProvenance> {
    let value = ModelProvenance {
        format_version: 1,
        provider: Provider::Mlx,
        review_id: loaded.review_id.clone(),
        runtime_plan_id: plan.id.clone(),
        settings_revision: expected_settings
            .checked_add(1)
            .filter(|r| *r <= MAX_REVISION)
            .ok_or_else(settings_error)?,
        worker_sha256: plan.worker.sha256.clone(),
        runtime_bundle_sha256: plan.runtime_bundle_sha256.clone(),
        package_sha256: manifest.package_sha256.clone(),
        template: manifest
            .template
            .ok_or_else(|| failure(mlx::Failure::UnsupportedCapability))?,
        thinking_mode: loaded.thinking_mode,
        sampling: ProvenanceSampling {
            max_tokens: loaded.sampling.max_tokens,
            temperature: loaded.sampling.temperature,
            top_p: loaded.sampling.top_p,
            seed: loaded.sampling.seed,
        },
    };
    value.validate()?;
    Ok(value)
}
fn check_generation_provenance(
    value: &ModelProvenance,
    actual: &mlx::GenerationProvenance,
) -> ApiResult<()> {
    value.validate()?;
    if actual.capability != mlx::Capability::Chat
        || actual.runtime_plan_id != value.runtime_plan_id
        || actual.worker_sha256 != value.worker_sha256
        || actual.runtime_bundle_sha256 != value.runtime_bundle_sha256
        || actual.package_sha256 != value.package_sha256
        || actual.template != Some(value.template)
        || actual.thinking_mode != Some(value.thinking_mode)
        || actual.sampling.max_tokens != value.sampling.max_tokens
        || actual.sampling.temperature != value.sampling.temperature
        || actual.sampling.top_p != value.sampling.top_p
        || actual.sampling.seed != value.sampling.seed
    {
        return Err(failure(mlx::Failure::ConsentMismatch));
    }
    Ok(())
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/mlx/status", get(status))
        .route("/api/mlx/prepare", post(prepare))
        .route("/api/mlx/load", post(load))
        .route("/api/mlx/cancel", post(cancel))
        .route("/api/mlx/unload", post(unload))
        .route("/api/mlx/completions", post(raw_completion))
}
async fn status(State(a): State<Arc<App>>) -> ApiResult<Json<Value>> {
    // Do not hold the MLX mutex while acquiring the database: send admission
    // already owns a transaction when it resolves a Destination.
    let settings = read_settings(&a.db.lock().unwrap())?;
    let inner = a.mlx.inner.lock().unwrap();
    Ok(Json(json!({
        "settings": settings,
        "runtime": inner.runtime.as_ref().map(mlx::Runtime::status),
        "loaded": inner.loaded.as_ref().map(|l|json!({"model_id":l.model_id,"endpoint":l.endpoint,"capability":l.capability,"sampling":l.sampling,"thinking_mode":l.thinking_mode,"review_id":l.review_id,"model_provenance":l.provenance})),
        "prepared": inner.prepared.as_ref().map(|p|json!({"plan":p.plan.summary(),"settings_revision":p.settings_revision,"revision":p.expected.revision,"model_id":p.loaded.model_id,"capability":p.loaded.capability,"sampling":p.loaded.sampling,"thinking_mode":p.loaded.thinking_mode,"review_id":p.loaded.review_id,"catalog":p.catalog.as_ref().map(|b|b.summary())})),
        "operation": inner.operation.as_ref().map(|o|json!({"id":o.id,"kind":o.kind,"status":o.status,"error":o.error,"config_revision":o.config_revision,"progress":o.progress,"completion":o.completion,"persisted":false})),
        "cleanup_unconfirmed":inner.cleanup_unconfirmed || inner.runtime.as_ref().is_some_and(|r|r.status().failure==Some(mlx::Failure::Cleanup)),"stopping":inner.stopping,
        "generation_reserved":a.mlx.generation.load(Ordering::Acquire),
        "streaming":true,"automatic_load":false,
        "thinking_modes":["model_default","disabled"]
    })))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerInput {
    path: String,
    source: String,
    source_revision: String,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    #[default]
    Native,
    CompatibleFixture,
}
impl Kind {
    fn runtime(self) -> ApiResult<mlx::ExecutionKind> {
        match self {
            Self::Native => Ok(mlx::ExecutionKind::Native),
            Self::CompatibleFixture if cfg!(any(test, feature = "synthetic-profile")) => {
                Ok(mlx::ExecutionKind::CompatibleFixture)
            }
            _ => Err(failure(mlx::Failure::UnsupportedPlatform)),
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareInput {
    worker: WorkerInput,
    model_directory: String,
    model_id: String,
    capability: mlx::Capability,
    sampling: Sampling,
    #[serde(default)]
    thinking_mode: mlx::ThinkingMode,
    settings_revision: u64,
    #[serde(default)]
    execution_kind: Kind,
    expected: Expected,
}
fn no_other_engine(a: &App) -> ApiResult<()> {
    if a.native.busy() || a.native.occupied() || a.acceleration.busy() {
        return Err(busy());
    }
    Ok(())
}
async fn prepare(
    State(a): State<Arc<App>>,
    Json(input): Json<PrepareInput>,
) -> ApiResult<Json<Value>> {
    prepare_selected(a, input, None).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogSetupInput {
    model_id: String,
    capability: mlx::Capability,
    sampling: Sampling,
    #[serde(default)]
    thinking_mode: mlx::ThinkingMode,
    settings_revision: u64,
    expected: Expected,
}
pub(crate) async fn prepare_catalog(
    a: Arc<App>,
    selected: Arc<local_catalog_api::PreparedCatalog>,
    settings: CatalogSetupInput,
) -> ApiResult<Json<Value>> {
    if selected.entry().manifest.description.format != smith_local_catalog::ModelFormat::Mlx {
        return Err(bad("Choose the setup adapter matching this catalog entry."));
    }
    let worker = selected.selected_file(true, smith_local_catalog::FileRole::Executable)?;
    let input = PrepareInput {
        worker: WorkerInput {
            path: worker.path,
            source: worker.source,
            source_revision: worker.source_revision,
        },
        model_directory: selected
            .entry()
            .manifest
            .model
            .root
            .clone()
            .into_os_string()
            .into_string()
            .map_err(|_| bad("The selected model directory is not valid UTF-8."))?,
        model_id: settings.model_id,
        capability: settings.capability,
        sampling: settings.sampling,
        thinking_mode: settings.thinking_mode,
        settings_revision: settings.settings_revision,
        execution_kind: Kind::Native,
        expected: settings.expected,
    };
    prepare_selected(a, input, Some(selected)).await
}
async fn prepare_selected(
    a: Arc<App>,
    input: PrepareInput,
    catalog: Option<Arc<local_catalog_api::PreparedCatalog>>,
) -> ApiResult<Json<Value>> {
    input.sampling.validate()?;
    if input.capability != mlx::Capability::Chat
        && input.thinking_mode != mlx::ThinkingMode::ModelDefault
    {
        return Err(failure(mlx::Failure::UnsupportedCapability));
    }
    if !model_id(&input.model_id)
        || input.worker.path.len() > 4096
        || input.model_directory.len() > 4096
    {
        return Err(failure(mlx::Failure::InvalidSelection));
    }
    let kind = input.execution_kind.runtime()?;
    let (operation, cancel) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        no_other_engine(&a)?;
        if let Some(selected) = &catalog {
            local_catalog_api::validate_binding_locked(&a, &consent, &selected.binding())?;
        }
        check_settings(&a.db.lock().unwrap(), input.settings_revision)?;
        if a.mlx.busy() || a.mlx.occupied() {
            return Err(busy());
        }
        let result = a.mlx.begin("prepare")?;
        a.mlx.inner.lock().unwrap().prepared = None;
        result
    };
    let response = json!({"operation_id":operation});
    let app = a.clone();
    a.spawn_task(async move {
        let result = async {
            let plan = mlx::LaunchPlan::prepare(
                mlx::SelectedWorker {
                    path: input.worker.path.clone().into(),
                    source: input.worker.source.clone(),
                    source_revision: input.worker.source_revision.clone(),
                    execution_kind: kind,
                },
                std::path::Path::new(&input.model_directory),
                mlx::Budgets::default(),
                &cancel,
            )
            .await
            .map_err(failure)?;
            if let Some(selected) = &catalog {
                selected.require_mlx(plan.summary())?;
            }
            let consent = app.consent.lock().unwrap();
            validate_expected(&consent, &app.config.lock().unwrap(), &input.expected)?;
            check_settings(&app.db.lock().unwrap(), input.settings_revision)?;
            no_other_engine(&app)?;
            if let Some(selected) = &catalog {
                local_catalog_api::validate_binding_locked(&app, &consent, &selected.binding())?;
            }
            if cancel.is_cancelled() {
                return Err(failure(mlx::Failure::Cancelled));
            }
            let mut inner = app.mlx.inner.lock().unwrap();
            if inner.stopping {
                return Err(busy());
            }
            let review_id = review_intent(
                &input,
                &plan.summary().id,
                catalog.as_ref().map(|p| p.binding()).as_ref(),
            )?;
            let endpoint = format!("mlx://{}", plan.summary().id);
            inner.prepared = Some(Prepared {
                plan,
                expected: input.expected,
                settings_revision: input.settings_revision,
                catalog: catalog.as_ref().map(|p| p.binding()),
                loaded: Loaded {
                    model_id: input.model_id,
                    endpoint,
                    capability: input.capability,
                    sampling: input.sampling,
                    thinking_mode: input.thinking_mode,
                    review_id,
                    provenance: None,
                },
            });
            if let Some(op) = inner.operation.as_mut() {
                op.status = "succeeded";
            }
            Ok(())
        }
        .await;
        app.mlx.finish(&operation, result);
    });
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadInput {
    plan_id: String,
    review_id: String,
    settings_revision: u64,
    confirm: bool,
    expected: Expected,
}
async fn load(State(a): State<Arc<App>>, Json(input): Json<LoadInput>) -> ApiResult<Json<Value>> {
    let (operation, cancel, mut prepared, approved, summary, catalog) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        no_other_engine(&a)?;
        check_settings(&a.db.lock().unwrap(), input.settings_revision)?;
        if !input.confirm {
            return Err(bad(
                "Review the exact selected files and confirm this MLX load plan.",
            ));
        }
        if a.mlx.busy() || a.mlx.occupied() {
            return Err(busy());
        }
        let mut inner = a.mlx.inner.lock().unwrap();
        let p = inner
            .prepared
            .as_ref()
            .ok_or_else(|| bad("Prepare and review an MLX plan first."))?;
        if p.plan.summary().id != input.plan_id
            || p.loaded.review_id != input.review_id
            || p.settings_revision != input.settings_revision
            || p.expected.revision != input.expected.revision
            || p.expected.provider != input.expected.provider
            || p.expected.endpoint != input.expected.endpoint
        {
            return Err(failure(mlx::Failure::ConsentMismatch));
        }
        if let Some(binding) = &p.catalog {
            local_catalog_api::validate_binding_locked(&a, &consent, binding)?;
        }
        let p = inner.prepared.take().unwrap();
        let summary = p.plan.summary().clone();
        let approved = p.plan.approve(&input.plan_id).map_err(failure)?;
        let loaded = p.loaded;
        let catalog = p.catalog;
        drop(inner);
        let (operation, cancel) = a.mlx.begin("load")?;
        (operation, cancel, loaded, approved, summary, catalog)
    };
    let response = json!({"operation_id":operation});
    let app = a.clone();
    a.spawn_task(async move {
        let old = app.mlx.inner.lock().unwrap().runtime.clone();
        let mut new_runtime = None;
        let mut result = async {
            if let Some(old) = old { cleanup(&app.mlx, &old).await?; }
            let runtime = match mlx::Runtime::launch_owned(approved, cancel.clone()).await {
                Ok(r) => r,
                Err(launch) => {
                    // Even a failed handshake owns a cleanup receipt. Keep it
                    // visible while the registered task confirms/retries cleanup.
                    let mut inner = app.mlx.inner.lock().unwrap();
                    inner.cleanup_unconfirmed = launch.error == mlx::Failure::Cleanup;
                    inner.runtime = Some(launch.runtime.clone());
                    new_runtime = Some(launch.runtime);
                    return Err(failure(launch.error));
                }
            };
            app.mlx.inner.lock().unwrap().runtime = Some(runtime.clone());
            new_runtime = Some(runtime.clone());
            let mut request = runtime.begin(runtime.status().revision, mlx::Request::Load).map_err(failure)?;
            let outcome = wait_operation(&mut request, &cancel).await.map_err(failure)?;
            let mlx::Outcome::Manifest(manifest) = outcome else { return Err(failure(mlx::Failure::Protocol)); };
            if manifest.files != summary.model.files || manifest.package_sha256 != summary.model.package_sha256
                || !manifest.capabilities().contains(&prepared.capability) {
                return Err(failure(mlx::Failure::UnsupportedCapability));
            }
            if prepared.sampling.max_tokens > manifest.context_limit { return Err(failure(mlx::Failure::InvalidRequest)); }
            let ready=runtime.status();
            if prepared.capability==mlx::Capability::Chat {
                runtime.validate_chat_thinking(ready.revision,prepared.thinking_mode).map_err(failure)?;
                prepared.provenance=Some(loaded_provenance(&prepared,&summary,&manifest,input.settings_revision)?);
            }
            let mut consent = app.consent.lock().unwrap();
            if let Some(binding)=&catalog {local_catalog_api::validate_binding_locked(&app,&consent,binding)?;}
            let _commit = app.mlx.commit_gate.lock().unwrap();
            let mut config = app.config.lock().unwrap();
            validate_expected(&consent, &config, &input.expected)?;
            no_other_engine(&app)?;
            if cancel.is_cancelled() || app.mlx.inner.lock().unwrap().stopping { return Err(failure(mlx::Failure::Cancelled)); }
            let snapshot = runtime.status();
            if snapshot.phase != mlx::Phase::Ready || !snapshot.accepting { return Err(failure(mlx::Failure::NotLoaded)); }
            let next = Config { provider: "mlx".into(), endpoint: prepared.endpoint.clone(), model: prepared.model_id.clone() };
            {
                let mut db = app.db.lock().unwrap();
                let tx = db.transaction()?;
                tx.execute("INSERT INTO settings(key,value) VALUES('config',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&next).unwrap()])?;
                save_settings(&tx, input.settings_revision, &prepared, SuccessRecord {
                    thinking_mode:prepared.thinking_mode,review_id:Some(prepared.review_id.clone()),
                    model_id: prepared.model_id.clone(), plan_id: summary.id, worker_sha256: summary.worker.sha256,
                    runtime_bundle_sha256: summary.runtime_bundle_sha256,
                    package_sha256: manifest.package_sha256, capability: prepared.capability,
                    template: manifest.template, context_limit: manifest.context_limit,
                })?;
                tx.commit()?;
            }
            *config = next;
            { let mut auth = app.auth.lock().unwrap(); auth.secret = None; auth.revision += 1; }
            *app.tested.lock().unwrap() = None;
            app.models.lock().unwrap().clear();
            *app.models_revision.lock().unwrap() = None;
            consent.advance();
            let mut inner = app.mlx.inner.lock().unwrap();
            inner.loaded = Some(prepared);
            if let Some(op) = inner.operation.as_mut().filter(|o| o.id == operation) {
                op.config_revision = Some(consent.token()); op.status = "succeeded";
            }
            Ok(())
        }.await;
        if result.is_err() {
            if let Some(runtime) = new_runtime {
                if let Err(error) = cleanup(&app.mlx, &runtime).await { result = Err(error); }
            }
        }
        app.mlx.finish(&operation, result);
    });
    Ok(Json(response))
}
async fn wait_operation(
    operation: &mut mlx::Operation,
    cancel: &mlx::Cancellation,
) -> mlx::Result<mlx::Outcome> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => { operation.cancel(); operation.wait().await },
        result = operation.wait() => result,
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    operation_id: String,
    expected: Expected,
}
async fn cancel(
    State(a): State<Arc<App>>,
    Json(input): Json<CancelInput>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    let mut inner = a.mlx.inner.lock().unwrap();
    let op = inner
        .operation
        .as_mut()
        .filter(|o| o.id == input.operation_id && o.status == "running")
        .ok_or_else(|| bad("This MLX operation is no longer cancellable."))?;
    if op.kind == "unload" {
        return Err(bad("Owned cleanup must finish before another operation."));
    }
    op.cancel_requested = true;
    op.cancel.cancel();
    Ok(Json(
        json!({"operation_id":op.id,"cancellation_requested":true}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnloadInput {
    session_id: String,
    runtime_revision: u64,
    expected: Expected,
}
fn selected_runtime(a: &App, session: &str, revision: u64) -> ApiResult<mlx::Runtime> {
    let inner = a.mlx.inner.lock().unwrap();
    let runtime = inner
        .runtime
        .as_ref()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    let s = runtime.status();
    if s.session_id != session || s.revision != revision {
        return Err(failure(mlx::Failure::StaleRevision));
    }
    Ok(runtime.clone())
}
async fn unload(
    State(a): State<Arc<App>>,
    Json(input): Json<UnloadInput>,
) -> ApiResult<Json<Value>> {
    let (operation, runtime) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        no_other_engine(&a)?;
        if a.mlx.busy() {
            return Err(busy());
        }
        let runtime = selected_runtime(&a, &input.session_id, input.runtime_revision)?;
        let (operation, _) = a.mlx.begin("unload")?;
        (operation, runtime)
    };
    let response = json!({"operation_id":operation});
    let app = a.clone();
    a.spawn_task(async move {
        let result = cleanup(&app.mlx, &runtime).await;
        app.mlx.finish(&operation, result);
    });
    Ok(Json(response))
}

pub fn ready(a: &App, c: &Config) -> ApiResult<(mlx::Runtime, mlx::Snapshot)> {
    let inner = a.mlx.inner.lock().unwrap();
    if inner.stopping || inner.cleanup_unconfirmed || running(&inner) {
        return Err(busy());
    }
    let loaded = inner
        .loaded
        .as_ref()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    let runtime = inner
        .runtime
        .as_ref()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    let snapshot = runtime.status();
    if c.provider != "mlx"
        || loaded.model_id != c.model
        || loaded.endpoint != c.endpoint
        || loaded.capability != mlx::Capability::Chat
        || !snapshot.accepting
        || !matches!(snapshot.phase, mlx::Phase::Ready | mlx::Phase::Generating)
        || !snapshot
            .manifest
            .as_ref()
            .is_some_and(|m| m.capabilities().contains(&mlx::Capability::Chat))
    {
        return Err(failure(mlx::Failure::NotLoaded));
    }
    Ok((runtime.clone(), snapshot))
}
pub fn models(a: &App, c: &Config) -> Vec<Value> {
    ready(a,c).map(|(_, s)| vec![json!({"id":c.model,"managed":true,"streaming":true,"backend":s.backend,"capabilities":["chat"],"package_sha256":s.manifest.unwrap().package_sha256})]).unwrap_or_default()
}
struct GenerationLease(Arc<AtomicBool>);
impl Drop for GenerationLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
pub struct Destination {
    runtime: mlx::Runtime,
    revision: u64,
    request: mlx::GenerationRequest,
    provenance: ModelProvenance,
    runtime_provenance: mlx::GenerationProvenance,
    _lease: GenerationLease,
}
impl Destination {
    pub(crate) fn provenance(&self) -> &ModelProvenance {
        &self.provenance
    }
}
/// Immutable live selection only; never reads settings while run admission owns its transaction.
pub(crate) fn selected_provenance(a: &App, c: &Config) -> ApiResult<ModelProvenance> {
    let (_, snapshot) = ready(a, c)?;
    let inner = a.mlx.inner.lock().unwrap();
    let loaded = inner
        .loaded
        .as_ref()
        .filter(|l| l.model_id == c.model && l.endpoint == c.endpoint)
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    let provenance = loaded
        .provenance
        .clone()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    provenance.validate()?;
    if snapshot.manifest.as_ref().is_none_or(|m| {
        m.package_sha256 != provenance.package_sha256 || m.template != Some(provenance.template)
    }) {
        return Err(failure(mlx::Failure::ConsentMismatch));
    }
    Ok(provenance)
}
fn messages(history: &[Value]) -> ApiResult<Vec<mlx::Message>> {
    if history.is_empty() || history.len() > 64 {
        return Err(failure(mlx::Failure::InvalidRequest));
    }
    let mut bytes = 0usize;
    let mut result = Vec::with_capacity(history.len());
    for message in history {
        let role = match message["role"].as_str() {
            Some("system") => mlx::Role::System,
            Some("user") => mlx::Role::User,
            Some("assistant") => mlx::Role::Assistant,
            _ => return Err(failure(mlx::Failure::UnsupportedCapability)),
        };
        let content = message["content"]
            .as_str()
            .ok_or_else(|| failure(mlx::Failure::InvalidRequest))?;
        bytes += content.len();
        if bytes > 65536 {
            return Err(failure(mlx::Failure::InvalidRequest));
        }
        result.push(mlx::Message {
            role,
            content: content.into(),
        });
    }
    if serde_json::to_vec(&result)
        .map_err(|_| failure(mlx::Failure::InvalidRequest))?
        .len()
        > mlx::FRAME_BYTES - 2048
    {
        return Err(failure(mlx::Failure::InvalidRequest));
    }
    Ok(result)
}
/// Caller holds app.consent and has validated the exact selected target.
/// This function does not acquire consent recursively or read the database.
pub fn destination(a: &App, c: &Config, history: &[Value]) -> ApiResult<Destination> {
    let messages = messages(history)?;
    let (runtime, snapshot) = ready(a, c)?;
    if snapshot.phase != mlx::Phase::Ready {
        return Err(busy());
    }
    let inner = a.mlx.inner.lock().unwrap();
    // Recheck under the same mutex as unload/setup admission. No database access.
    if inner.stopping || inner.cleanup_unconfirmed || running(&inner) {
        return Err(busy());
    }
    let loaded = inner
        .loaded
        .as_ref()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    if loaded.endpoint != c.endpoint || loaded.model_id != c.model {
        return Err(failure(mlx::Failure::NotLoaded));
    }
    let provenance = loaded
        .provenance
        .clone()
        .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
    let request = mlx::GenerationRequest::ChatWithThinking {
        messages,
        sampling: loaded.sampling.runtime(),
        thinking_mode: loaded.thinking_mode,
    };
    let runtime_provenance = runtime
        .generation_provenance(snapshot.revision, &request)
        .map_err(failure)?;
    check_generation_provenance(&provenance, &runtime_provenance)?;
    a.mlx
        .generation
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| busy())?;
    Ok(Destination {
        runtime,
        revision: snapshot.revision,
        request,
        provenance,
        runtime_provenance,
        _lease: GenerationLease(a.mlx.generation.clone()),
    })
}
fn overwrite(a: &App, run: &Run, text: &str) -> Result<(), String> {
    if text.len() > mlx::OUTPUT_BYTES {
        return Err(failure(mlx::Failure::OutputLimit).1);
    }
    let mut state = run.tx.borrow().clone();
    state.content = text.to_owned();
    state.transport_incomplete = true;
    if persist(a, &state).is_err() {
        state.persisted = false;
        run.tx.send_replace(state);
        return Err("Cannot save the latest MLX response. Copy its visible text before leaving; the saved answer may be incomplete.".into());
    }
    run.tx.send_replace(state);
    Ok(())
}
pub async fn generate(
    a: &App,
    run: &Run,
    destination: Destination,
    _history: Vec<Value>,
) -> Result<streaming::Outcome, String> {
    // Destination holds the exact admitted messages, not a later mutable history.
    let mut operation = destination
        .runtime
        .begin(
            destination.revision,
            mlx::Request::Generate(destination.request.clone()),
        )
        .map_err(|e| failure(e).1)?;
    let mut changes = operation.subscribe();
    let mut cancelled = false;
    let mut last_text = String::new();
    let mut save_error = overwrite(a, run, "").err();
    if save_error.is_some() {
        operation.cancel();
        cancelled = true;
    }
    let result = loop {
        tokio::select! {
            biased;
            _ = run.cancel.notified(), if !cancelled => { operation.cancel(); cancelled = true; },
            result = operation.wait() => break result,
            changed = changes.changed() => {
                if changed.is_err() { continue; }
                let progress = changes.borrow_and_update().clone();
                if progress.text != last_text {
                    last_text = progress.text.clone();
                    if let Err(error) = overwrite(a, run, &progress.text) {
                        if save_error.is_none() { save_error = Some(error); }
                        operation.cancel(); cancelled = true;
                    }
                }
            }
        }
    };
    let final_progress = operation.snapshot();
    if let Err(error) = overwrite(a, run, &final_progress.text) {
        if save_error.is_none() {
            save_error = Some(error);
        }
    }
    if let Some(error) = save_error {
        return Err(error);
    }
    let completion = match result.map_err(|e| failure(e).1)? {
        mlx::Outcome::Completion(c) => c,
        _ => return Err(failure(mlx::Failure::Protocol).1),
    };
    if operation.provenance() != Some(&destination.runtime_provenance) {
        return Err(failure(mlx::Failure::Protocol).1);
    }
    if completion.text != final_progress.text {
        overwrite(a, run, &completion.text)?;
    }
    let incomplete =
        completion.incomplete || completion.finish_reason == mlx::FinishReason::Cancelled;
    Ok(streaming::Outcome {
        status: if incomplete { "cancelled" } else if completion.finish_reason == mlx::FinishReason::Length { "limited" } else { "completed" }.into(),
        notice: incomplete.then(|| "MLX generation stopped with an incomplete answer. Its worker was retired; explicitly load again.".into()),
        finish_reason: match completion.finish_reason { mlx::FinishReason::Stop => "stop", mlx::FinishReason::Length => "length", mlx::FinishReason::Cancelled => "cancelled" }.into(),
        transport_incomplete: incomplete,
    })
}
pub async fn task_generate(
    destination: Destination,
    _history: Vec<Value>,
    cancel: mlx::Cancellation,
) -> mlx::Result<mlx::Completion> {
    let mut operation = destination.runtime.begin(
        destination.revision,
        mlx::Request::Generate(destination.request.clone()),
    )?;
    let outcome = wait_operation(&mut operation, &cancel).await?;
    if operation.provenance() != Some(&destination.runtime_provenance) {
        return Err(mlx::Failure::Protocol);
    }
    match outcome {
        mlx::Outcome::Completion(c) => Ok(c),
        _ => Err(mlx::Failure::Protocol),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    session_id: String,
    runtime_revision: u64,
    prompt: String,
    sampling: Sampling,
    confirm: bool,
    expected: Expected,
}
async fn raw_completion(
    State(a): State<Arc<App>>,
    Json(input): Json<RawInput>,
) -> ApiResult<Json<Value>> {
    input.sampling.validate()?;
    if !input.confirm
        || input.prompt.is_empty()
        || input.prompt.len() > 65536
        || serde_json::to_vec(&input.prompt)
            .map_err(|_| failure(mlx::Failure::InvalidRequest))?
            .len()
            > mlx::FRAME_BYTES - 2048
    {
        return Err(failure(mlx::Failure::InvalidRequest));
    }
    let (id, cancel, runtime) = {
        let consent = a.consent.lock().unwrap();
        let config = a.config.lock().unwrap();
        validate_expected(&consent, &config, &input.expected)?;
        no_other_engine(&a)?;
        if a.mlx.busy() {
            return Err(busy());
        }
        let runtime = selected_runtime(&a, &input.session_id, input.runtime_revision)?;
        {
            let inner = a.mlx.inner.lock().unwrap();
            let loaded = inner
                .loaded
                .as_ref()
                .ok_or_else(|| failure(mlx::Failure::NotLoaded))?;
            if loaded.capability != mlx::Capability::RawCompletion
                || config.provider != "mlx"
                || config.endpoint != loaded.endpoint
                || config.model != loaded.model_id
            {
                return Err(failure(mlx::Failure::UnsupportedCapability));
            }
        }
        let (id, cancel) = a.mlx.begin("raw_completion")?;
        (id, cancel, runtime)
    };
    let response = json!({"operation_id":id,"persisted":false});
    let app = a.clone();
    a.spawn_task(async move {
        let result = async {
            let mut operation = runtime.begin(input.runtime_revision,mlx::Request::Generate(mlx::GenerationRequest::Raw {
                prompt:input.prompt,sampling:input.sampling.runtime(),
            })).map_err(failure)?;
            let mut changes = operation.subscribe();
            let mut cancellation_sent = false;
            let outcome = loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled(), if !cancellation_sent => { operation.cancel(); cancellation_sent=true; },
                    result = operation.wait() => break result,
                    changed = changes.changed() => {
                        if changed.is_ok() {
                            let progress = changes.borrow_and_update().clone();
                            if let Some(op)=app.mlx.inner.lock().unwrap().operation.as_mut().filter(|o|o.id==id) { op.progress=Some(progress); }
                        }
                    }
                }
            };
            {
                let mut inner=app.mlx.inner.lock().unwrap();
                if let Some(op)=inner.operation.as_mut().filter(|o|o.id==id) { op.progress=Some(operation.snapshot()); }
            }
            match outcome.map_err(failure)? {
                mlx::Outcome::Completion(c) => {
                    let incomplete=c.incomplete;
                    if let Some(op)=app.mlx.inner.lock().unwrap().operation.as_mut().filter(|o|o.id==id) { op.completion=Some(c); }
                    if incomplete { return Err(failure(mlx::Failure::Cancelled)); }
                    Ok(())
                },
                _ => Err(failure(mlx::Failure::Protocol)),
            }
        }.await;
        app.mlx.finish(&id,result);
    });
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Method, Request as HttpRequest},
    };
    use futures_util::FutureExt;
    use smith_profile_storage as storage;
    use std::{
        path::{Path as FilePath, PathBuf},
        sync::OnceLock,
    };
    use tower::ServiceExt;
    const SECRET: &str = "synthetic MLX adapter profile secret";
    static WORKER: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    fn worker() -> &'static FilePath {
        &WORKER
            .get_or_init(|| {
                let d = tempfile::tempdir().unwrap();
                let p = d.path().join("synthetic-mlx-worker");
                assert!(std::process::Command::new(
                    std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())
                )
                .arg(
                    FilePath::new(env!("CARGO_MANIFEST_DIR"))
                        .join("crates/smith-mlx-runtime/tests/fixtures/worker.rs")
                )
                .args(["--edition=2021", "-o"])
                .arg(&p)
                .status()
                .unwrap()
                .success());
                (d, p)
            })
            .1
    }
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        app: Option<Arc<App>>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let db = storage::create(
                &root.join("profile.sqlite3"),
                &storage::Secret::new(SECRET.into()).unwrap(),
            )
            .unwrap();
            Self {
                _dir: dir,
                root,
                app: Some(app_from_connection(db).unwrap()),
            }
        }
        fn app(&self) -> Arc<App> {
            self.app.as_ref().unwrap().clone()
        }
        fn call(&self, method: Method, path: &str, input: Value) -> (StatusCode, Value) {
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
            let response = router(self.app())
                .oneshot(request)
                .now_or_never()
                .expect("route must register work before awaiting")
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
                .now_or_never()
                .unwrap()
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes)
                    .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
            )
        }
        fn ok(&self, method: Method, path: &str, input: Value) -> Value {
            let (status, body) = self.call(method, path, input);
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            body
        }
        fn status(&self) -> Value {
            self.ok(Method::GET, "/api/mlx/status", Value::Null)
        }
        fn expected(&self) -> Value {
            let c = self.ok(Method::GET, "/api/config", Value::Null);
            json!({"revision":c["revision"],"provider":c["provider"],"endpoint":c["endpoint"]})
        }
        async fn drain(&self) {
            tokio::time::timeout(Duration::from_secs(20), self.app().tasks.drain())
                .await
                .unwrap();
        }
        async fn close(&mut self) {
            let app = self.app.take().unwrap();
            app.mlx.begin_shutdown();
            self::tokio_drain(&app).await;
            app.mlx.shutdown().await.unwrap();
            let owned =
                Arc::try_unwrap(app).unwrap_or_else(|_| panic!("owned app retained after drain"));
            storage::close(owned.db.into_inner().unwrap())
                .unwrap_or_else(|_| panic!("synthetic close failed"));
        }
        async fn restart(&mut self) {
            self.close().await;
            let db = storage::unlock(
                &self.root.join("profile.sqlite3"),
                &storage::Secret::new(SECRET.into()).unwrap(),
            )
            .unwrap();
            self.app = Some(app_from_connection(db).unwrap());
        }
        fn input(&self, mode: &str, capability: &str) -> Value {
            let selected = self.root.join(mode);
            std::fs::copy(worker(), &selected).unwrap();
            let model = self.root.join(format!("{mode}-model"));
            std::fs::create_dir(&model).unwrap();
            for name in [
                "config.json",
                "tokenizer.json",
                "tokenizer_config.json",
                "model.safetensors",
            ] {
                std::fs::write(model.join(name), format!("synthetic {name}")).unwrap();
            }
            json!({"worker":{"path":selected,"source":"synthetic owned protocol fixture","source_revision":"fixture-v1"},"model_directory":model,"model_id":"synthetic-mlx","capability":capability,"sampling":{"max_tokens":128,"temperature":0.0,"top_p":1.0},"settings_revision":self.status()["settings"]["revision"],"execution_kind":"compatible_fixture","expected":self.expected()})
        }
        async fn prepare(&self, mode: &str, capability: &str, chat_manifest: bool) -> Value {
            let input = self.input(mode, capability);
            self.ok(Method::POST, "/api/mlx/prepare", input);
            self.drain().await;
            let status = self.status();
            assert_eq!(status["operation"]["status"], "succeeded", "{status}");
            let p = &status["prepared"];
            let manifest = json!({"model_type":"llama","files":p["plan"]["model"]["files"],"package_sha256":p["plan"]["model"]["package_sha256"],"template":if chat_manifest{json!("simple-chatml-v1")}else{Value::Null},"context_limit":4096});
            std::fs::write(self.root.join(mode).with_extension("manifest"),json!({"manifest":manifest,"capabilities":if chat_manifest{json!(["raw_completion","chat"])}else{json!(["raw_completion"])}}).to_string()).unwrap();
            p.clone()
        }
        async fn load(&self, p: &Value) {
            self.ok(Method::POST,"/api/mlx/load",json!({"plan_id":p["plan"]["id"],"review_id":p["review_id"],"settings_revision":p["settings_revision"],"confirm":true,"expected":self.expected()}));
            self.drain().await;
        }
        async fn unload(&self) {
            let s = self.status();
            self.ok(Method::POST,"/api/mlx/unload",json!({"session_id":s["runtime"]["session_id"],"runtime_revision":s["runtime"]["revision"],"expected":self.expected()}));
            self.drain().await;
        }
    }
    async fn tokio_drain(app: &Arc<App>) {
        tokio::time::timeout(Duration::from_secs(20), app.tasks.drain())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_settings_defaults_strict_rows_atomic_commit_and_restart_are_passive() {
        let mut f = Fixture::new();
        let s = f.status();
        assert_eq!(s["settings"]["revision"], 0);
        assert!(s["runtime"].is_null());
        assert_eq!(
            f.app()
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM settings WHERE key='mlx_settings'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let p = f.prepare("normal", "chat", true).await;
        f.load(&p).await;
        let s = f.status();
        assert_eq!(s["operation"]["status"], "succeeded", "{s}");
        assert_eq!(s["settings"]["revision"], 1);
        let saved = s["settings"].clone();
        let config = f.app().config.lock().unwrap().clone();
        f.restart().await;
        assert_eq!(f.status()["settings"], saved);
        assert!(f.status()["runtime"].is_null());
        assert!(ready(&f.app(), &config).is_err());
        let raw: String = f
            .app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='mlx_settings'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for malformed in ["{", "{\"format_version\":2}", "null"] {
            f.app()
                .db
                .lock()
                .unwrap()
                .execute(
                    "UPDATE settings SET value=?1 WHERE key='mlx_settings'",
                    [malformed],
                )
                .unwrap();
            assert_eq!(
                f.call(Method::GET, "/api/mlx/status", Value::Null).0,
                StatusCode::CONFLICT
            );
            assert_eq!(
                f.app()
                    .db
                    .lock()
                    .unwrap()
                    .query_row(
                        "SELECT value FROM settings WHERE key='mlx_settings'",
                        [],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                malformed
            );
        }
        f.app()
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE settings SET value=?1 WHERE key='mlx_settings'",
                [raw],
            )
            .unwrap();
        let bytes = std::fs::read(f.root.join("profile.sqlite3")).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"));
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_prepare_never_executes_and_exact_nested_consent_rejects_stale_or_extra_fields() {
        let mut f = Fixture::new();
        let input = f.input("normal", "chat");
        let before = f.app().config.lock().unwrap().clone();
        let mut invalid = input.clone();
        invalid["sampling"]["gpu_layers"] = json!(1);
        assert_eq!(
            f.call(Method::POST, "/api/mlx/prepare", invalid).0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut invalid = input.clone();
        invalid["sampling"]
            .as_object_mut()
            .unwrap()
            .remove("temperature");
        assert_eq!(
            f.call(Method::POST, "/api/mlx/prepare", invalid).0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let started = f.ok(Method::POST, "/api/mlx/prepare", input.clone());
        assert_eq!(
            f.call(Method::POST, "/api/mlx/prepare", input).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            f.call(
                Method::POST,
                "/api/mlx/cancel",
                json!({"operation_id":"wrong","expected":f.expected()})
            )
            .0,
            StatusCode::BAD_REQUEST
        );
        f.drain().await;
        let s = f.status();
        assert_eq!(s["operation"]["id"], started["operation_id"]);
        assert_eq!(s["operation"]["status"], "succeeded");
        assert!(!f.root.join("normal.pid").exists());
        assert!(!f.root.join("normal.scratch").exists());
        let p = s["prepared"].clone();
        for (id, confirm, revision) in [
            (json!("wrong"), true, json!(0)),
            (p["plan"]["id"].clone(), false, json!(0)),
            (p["plan"]["id"].clone(), true, json!(1)),
        ] {
            assert_ne!(f.call(Method::POST,"/api/mlx/load",json!({"plan_id":id,"review_id":p["review_id"],"confirm":confirm,"settings_revision":revision,"expected":f.expected()})).0,StatusCode::OK);
            assert!(!f.root.join("normal.pid").exists());
        }
        let mut stale = f.expected();
        stale["revision"] = json!("old-session:0");
        assert_eq!(f.call(Method::POST,"/api/mlx/load",json!({"plan_id":p["plan"]["id"],"review_id":p["review_id"],"confirm":true,"settings_revision":0,"expected":stale})).0,StatusCode::CONFLICT);
        assert!(f.app().config.lock().unwrap().eq(&before));
        assert!(f.status()["runtime"].is_null());
        std::fs::write(
            f.root.join("normal-model/model.safetensors"),
            b"changed synthetic bytes",
        )
        .unwrap();
        f.load(&p).await;
        assert_eq!(f.status()["operation"]["status"], "failed");
        assert!(
            !f.root.join("normal.pid").exists(),
            "changed bytes must fail before execution"
        );
        assert_eq!(f.status()["settings"]["revision"], 0);
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_chat_router_persists_revised_utf8_and_reservation_blocks_unload() {
        let mut f = Fixture::new();
        let p = f.prepare("normal", "chat", true).await;
        f.load(&p).await;
        let app = f.app();
        let config = app.config.lock().unwrap().clone();
        let history = vec![json!({"role":"user","content":"synthetic prompt"})];
        let reserved = destination(&app, &config, &history).unwrap();
        assert!(app.mlx.busy());
        assert!(ready(&app, &config).is_ok());
        let s = f.status();
        assert_eq!(f.call(Method::POST,"/api/mlx/unload",json!({"session_id":s["runtime"]["session_id"],"runtime_revision":s["runtime"]["revision"],"expected":f.expected()})).0,StatusCode::CONFLICT);
        assert!(destination(&app, &config, &history).is_err());
        drop(reserved);
        assert!(!app.mlx.busy());
        assert!(destination(
            &app,
            &config,
            &[json!({"role":"tool","content":"must not coerce"})]
        )
        .is_err());
        assert!(destination(
            &app,
            &config,
            &[json!({"role":"user","content":"x".repeat(65537)})]
        )
        .is_err());
        let chat = f.ok(Method::POST, "/api/chats", f.expected());
        let mut send = f.expected().as_object().unwrap().clone();
        send.insert("content".into(), json!("synthetic revised response"));
        send.insert("version".into(), chat["version"].clone());
        f.ok(
            Method::POST,
            &format!("/api/chats/{}/messages", chat["id"].as_str().unwrap()),
            Value::Object(send),
        );
        f.drain().await;
        let result:(String,String,i64)=app.db.lock().unwrap().query_row("SELECT m.content,m.status,r.transport_incomplete FROM messages m JOIN run_configs r ON r.run_id=m.run_id WHERE m.chat_id=?1",[chat["id"].as_str().unwrap()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(result, ("hello 🌍".into(), "completed".into(), 0));
        f.unload().await;
        assert!(!app.mlx.occupied());
        drop(app);
        f.restart().await;
        assert!(f.status()["runtime"].is_null());
        assert_eq!(f.status()["settings"]["revision"], 1);
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_raw_only_load_never_admits_chat_and_raw_result_is_explicit() {
        let mut f = Fixture::new();
        let before = f.app().config.lock().unwrap().clone();
        let p = f.prepare("normal", "chat", false).await;
        f.load(&p).await;
        let status = f.status();
        assert_eq!(status["operation"]["status"], "failed");
        assert!(status["runtime"].is_null());
        assert!(f.app().config.lock().unwrap().eq(&before));
        assert_eq!(status["settings"]["revision"], 0);
        let p = f.prepare("raw", "raw_completion", false).await;
        f.load(&p).await;
        let s = f.status();
        assert_eq!(s["operation"]["status"], "succeeded", "{s}");
        let c = f.app().config.lock().unwrap().clone();
        assert!(models(&f.app(), &c).is_empty());
        assert!(destination(
            &f.app(),
            &c,
            &[json!({"role":"user","content":"no implicit template"})]
        )
        .is_err());
        f.ok(Method::POST,"/api/mlx/completions",json!({"session_id":s["runtime"]["session_id"],"runtime_revision":s["runtime"]["revision"],"prompt":"exact synthetic raw prompt","sampling":{"max_tokens":128,"temperature":0.0,"top_p":1.0},"confirm":true,"expected":f.expected()}));
        f.drain().await;
        let s = f.status();
        assert_eq!(s["operation"]["completion"]["text"], "hello 🌍");
        assert_eq!(s["operation"]["persisted"], false);
        assert_eq!(s["operation"]["status"], "succeeded");
        assert_eq!(
            f.app()
                .db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_commit_failure_reaps_new_worker_and_preserves_previous_config_and_settings() {
        let mut f = Fixture::new();
        let before = f.app().config.lock().unwrap().clone();
        let p = f.prepare("normal", "chat", true).await;
        f.app().db.lock().unwrap().execute_batch("CREATE TEMP TRIGGER reject_mlx_settings BEFORE INSERT ON settings WHEN NEW.key='mlx_settings' BEGIN SELECT RAISE(ABORT,'synthetic commit failure'); END;").unwrap();
        f.load(&p).await;
        let s = f.status();
        assert_eq!(s["operation"]["status"], "failed", "{s}");
        assert!(s["runtime"].is_null());
        assert!(!f.app().mlx.occupied());
        assert!(f.app().config.lock().unwrap().eq(&before));
        assert_eq!(s["settings"]["revision"], 0);
        assert_eq!(
            f.app()
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM settings WHERE key='config'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let scratch = std::fs::read_to_string(f.root.join("normal.scratch")).unwrap();
        assert!(!FilePath::new(&scratch).exists());
        f.app()
            .db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_mlx_settings;")
            .unwrap();
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_profile_revocation_preserves_active_partial_until_chat_cancellation_drains() {
        let mut f = Fixture::new();
        let p = f.prepare("stall", "chat", true).await;
        f.load(&p).await;
        let app = f.app();
        let config = app.config.lock().unwrap().clone();
        let history = vec![json!({"role":"user","content":"synthetic cancellation"})];
        let destination = destination(&app, &config, &history).unwrap();
        let selected =
            crate::task_model::selection(&app, &serde_json::from_value(f.expected()).unwrap())
                .unwrap();
        let cancel = mlx::Cancellation::new();
        let completion = task_generate(destination, history, cancel.clone());
        tokio::pin!(completion);
        let runtime = app.mlx.inner.lock().unwrap().runtime.clone().unwrap();
        let mut states = runtime.subscribe();
        tokio::select! { biased; result=&mut completion=>panic!("stalled fixture finished early: {result:?}"), _=async {while states.borrow_and_update().phase!=mlx::Phase::Generating {states.changed().await.unwrap();}}=>{} }
        assert!(
            crate::task_model::validate_selection(&app, &selected).is_ok(),
            "operation revisions must not invalidate task binding"
        );
        app.mlx.begin_shutdown();
        assert!(
            runtime.status().accepting,
            "revocation must let chat own its cancellation/persistence"
        );
        assert!(ready(&app, &config).is_err());
        cancel.cancel();
        let partial = tokio::time::timeout(Duration::from_secs(12), completion)
            .await
            .unwrap()
            .unwrap();
        assert!(partial.incomplete);
        assert_eq!(partial.finish_reason, mlx::FinishReason::Cancelled);
        app.mlx.shutdown().await.unwrap();
        assert!(!app.mlx.occupied());
        drop(app);
        f.close().await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_late_cleanup_fault_requires_final_shutdown_after_publishers_drain() {
        let f = Fixture::new();
        let app = f.app();
        let (release, ready) = tokio::sync::oneshot::channel::<()>();
        let publisher = app.clone();
        app.spawn_task(async move {
            ready.await.unwrap();
            publisher.mlx.inject_cleanup_fault_for_test();
        });
        assert!(
            app.mlx.shutdown().await.is_ok(),
            "unpublished actor is not visible yet"
        );
        release.send(()).unwrap();
        tokio_drain(&app).await;
        assert!(
            app.mlx.shutdown().await.is_err(),
            "final decision must observe late cleanup uncertainty"
        );
        assert!(app.mlx.occupied());
        assert!(app.mlx.busy());
        assert!(
            app.db
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM settings", [], |r| r.get::<_, i64>(0))
                .is_ok(),
            "owned connection stays available to the caller that must retain the faulted profile"
        );
        // No child was created in this injection test; drop the isolated fixture.
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mlx_protocol_failure_keeps_last_valid_partial_chat_text_incomplete() {
        let mut f = Fixture::new();
        let p = f.prepare("bad_boundary", "chat", true).await;
        f.load(&p).await;
        let chat = f.ok(Method::POST, "/api/chats", f.expected());
        let mut send = f.expected().as_object().unwrap().clone();
        send.insert("content".into(), json!("synthetic failure response"));
        send.insert("version".into(), chat["version"].clone());
        f.ok(
            Method::POST,
            &format!("/api/chats/{}/messages", chat["id"].as_str().unwrap()),
            Value::Object(send),
        );
        f.drain().await;
        let result:(String,String,i64)=f.app().db.lock().unwrap().query_row("SELECT m.content,m.status,r.transport_incomplete FROM messages m JOIN run_configs r ON r.run_id=m.run_id WHERE m.chat_id=?1",[chat["id"].as_str().unwrap()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(result, ("hél".into(), "failed".into(), 1));
        assert!(!f.app().mlx.occupied());
        f.close().await;
    }
    const THINKING_TEXT: &str = "<think>\n\n</think>\n\nLiteral fixture response.";

    impl Fixture {
        async fn prepare_thinking(&self, mode: &str, thinking: &str, template: &str) -> Value {
            let mut input = self.input(mode, "chat");
            input["thinking_mode"] = json!(thinking);
            self.ok(Method::POST, "/api/mlx/prepare", input);
            self.drain().await;
            let status = self.status();
            assert_eq!(status["operation"]["status"], "succeeded", "{status}");
            let p = status["prepared"].clone();
            let manifest = json!({"model_type":"qwen3","files":p["plan"]["model"]["files"],"package_sha256":p["plan"]["model"]["package_sha256"],"template":template,"context_limit":4096});
            std::fs::write(
                self.root.join(mode).with_extension("manifest"),
                json!({"manifest":manifest,"capabilities":["raw_completion","chat"]}).to_string(),
            )
            .unwrap();
            p
        }
        fn chat(&self) -> Value {
            let chat = self.ok(Method::POST, "/api/chats", self.expected());
            let mut send = self.expected().as_object().unwrap().clone();
            send.insert(
                "content".into(),
                json!("Synthetic thinking-control request"),
            );
            send.insert("version".into(), chat["version"].clone());
            let mut run = self.ok(
                Method::POST,
                &format!("/api/chats/{}/messages", chat["id"].as_str().unwrap()),
                Value::Object(send),
            );
            run["chat_id"] = chat["id"].clone();
            run
        }
        fn run_record(&self, run: &Value) -> (String, String, i64, Value) {
            let app = self.app();
            let db = app.db.lock().unwrap();
            let id = run["run_id"].as_str().unwrap();
            let (text, status, incomplete) = db.query_row("SELECT m.content,m.status,r.transport_incomplete FROM messages m JOIN run_configs r ON r.run_id=m.run_id WHERE m.run_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            let provenance = crate::inference_provenance::read(&db, id, "mlx")
                .unwrap()
                .unwrap();
            (
                text,
                status,
                incomplete,
                serde_json::to_value(provenance).unwrap(),
            )
        }
        fn replace_settings(&self, value: &Value) {
            self.app()
                .db
                .lock()
                .unwrap()
                .execute(
                    "UPDATE settings SET value=?1 WHERE key='mlx_settings'",
                    [value.to_string()],
                )
                .unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_thinking_review_is_exact_required_and_rejects_invalid_modes_before_execution() {
        let mut f = Fixture::new();
        let input = f.input("thinking_review", "chat");
        for mode in [Value::Null, json!(true), json!("auto")] {
            let mut invalid = input.clone();
            invalid["thinking_mode"] = mode;
            let (status, body) = f.call(Method::POST, "/api/mlx/prepare", invalid);
            assert!(
                matches!(
                    status,
                    StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
                ),
                "{status}: {body}"
            );
        }
        let mut raw = input.clone();
        raw["capability"] = json!("raw_completion");
        raw["thinking_mode"] = json!("disabled");
        assert_ne!(
            f.call(Method::POST, "/api/mlx/prepare", raw).0,
            StatusCode::OK
        );
        assert!(f.status()["operation"].is_null());
        let parsed: PrepareInput = serde_json::from_value(input.clone()).unwrap();
        let first = review_intent(&parsed, &"a".repeat(64), None).unwrap();
        for (key, value) in [
            ("thinking_mode", json!("disabled")),
            ("model_id", json!("other-label")),
            ("settings_revision", json!(1)),
            (
                "sampling",
                json!({"max_tokens":129,"temperature":0.0,"top_p":1.0}),
            ),
            (
                "expected",
                json!({"revision":"other:0","provider":"mock","endpoint":"http://127.0.0.1:11434"}),
            ),
        ] {
            let mut changed = input.clone();
            changed[key] = value;
            let changed: PrepareInput = serde_json::from_value(changed).unwrap();
            assert_ne!(
                first,
                review_intent(&changed, &"a".repeat(64), None).unwrap(),
                "{key}"
            );
        }
        f.ok(Method::POST, "/api/mlx/prepare", input);
        f.drain().await;
        let p = f.status()["prepared"].clone();
        let mut load = json!({"plan_id":p["plan"]["id"],"settings_revision":0,"confirm":true,"expected":f.expected()});
        assert_eq!(
            f.call(Method::POST, "/api/mlx/load", load.clone()).0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        load["review_id"] = json!("0".repeat(64));
        assert_eq!(
            f.call(Method::POST, "/api/mlx/load", load).0,
            StatusCode::CONFLICT
        );
        assert_eq!(f.status()["prepared"]["review_id"], p["review_id"]);
        assert!(!f.root.join("thinking_review.pid").exists());
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_thinking_settings_require_new_fields_and_only_legacy_shape_defaults() {
        let mut f = Fixture::new();
        let p = f
            .prepare_thinking("thinking_settings", "disabled", "qwen3-default-thinking-v1")
            .await;
        f.load(&p).await;
        let saved = f.status()["settings"].clone();
        assert_eq!(saved["format_version"], 2);
        assert_eq!(saved["thinking_mode"], "disabled");
        assert_eq!(saved["last_successful"]["review_id"], p["review_id"]);
        f.unload().await;
        let mut missing_both = saved.clone();
        missing_both
            .as_object_mut()
            .unwrap()
            .remove("thinking_mode");
        missing_both["last_successful"]
            .as_object_mut()
            .unwrap()
            .remove("thinking_mode");
        let mut null_mode = saved.clone();
        null_mode["thinking_mode"] = Value::Null;
        let mut missing_review = saved.clone();
        missing_review["last_successful"]
            .as_object_mut()
            .unwrap()
            .remove("review_id");
        let mut downgrade = saved.clone();
        downgrade["format_version"] = json!(1);
        for bad in [missing_both, null_mode, missing_review, downgrade] {
            f.replace_settings(&bad);
            assert_eq!(
                f.call(Method::GET, "/api/mlx/status", Value::Null).0,
                StatusCode::CONFLICT
            );
            let raw: String = f
                .app()
                .db
                .lock()
                .unwrap()
                .query_row(
                    "SELECT value FROM settings WHERE key='mlx_settings'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), bad);
        }
        // Exactly the historical version-1 shape: no thinking intent was stored.
        let mut legacy = saved.clone();
        legacy["format_version"] = json!(1);
        legacy.as_object_mut().unwrap().remove("thinking_mode");
        legacy["last_successful"]
            .as_object_mut()
            .unwrap()
            .remove("thinking_mode");
        legacy["last_successful"]
            .as_object_mut()
            .unwrap()
            .remove("review_id");
        f.replace_settings(&legacy);
        let legacy_status = f.status();
        assert_eq!(legacy_status["settings"]["format_version"], 1);
        assert_eq!(
            legacy_status["thinking_modes"],
            json!(["model_default", "disabled"])
        );
        assert_eq!(legacy_status["settings"]["thinking_mode"], "model_default");
        assert!(legacy_status["settings"]["last_successful"]["review_id"].is_null());
        assert!(
            legacy_status["runtime"].is_null(),
            "advertising host controls never starts a worker"
        );
        let raw: String = f
            .app()
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key='mlx_settings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            legacy,
            "status does not migrate or rewrite a legacy record"
        );
        f.replace_settings(&saved);
        f.restart().await;
        assert_eq!(f.status()["settings"], saved);
        assert!(f.status()["runtime"].is_null());
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_thinking_unsupported_feature_or_template_preserves_configuration_and_reaps() {
        for (mode, template) in [
            ("thinking_legacy", "qwen3-default-thinking-v1"),
            ("thinking_wrong_template", "simple-chatml-v1"),
        ] {
            let mut f = Fixture::new();
            let before = f.app().config.lock().unwrap().clone();
            let p = f.prepare_thinking(mode, "disabled", template).await;
            f.load(&p).await;
            let s = f.status();
            assert_eq!(s["operation"]["status"], "failed", "{s}");
            assert_eq!(s["settings"]["revision"], 0);
            assert!(f.app().config.lock().unwrap().eq(&before));
            assert!(!f.app().mlx.occupied());
            let scratch =
                std::fs::read_to_string(f.root.join(mode).with_extension("scratch")).unwrap();
            assert!(!FilePath::new(&scratch).exists());
            f.close().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_thinking_literal_chat_and_immutable_run_and_task_provenance_survive_settings_and_restart(
    ) {
        let mut f = Fixture::new();
        let p = f
            .prepare_thinking("thinking_success", "disabled", "qwen3-default-thinking-v1")
            .await;
        f.load(&p).await;
        assert_eq!(f.status()["operation"]["status"], "succeeded");
        let provenance = f.status()["loaded"]["model_provenance"].clone();
        assert_eq!(provenance["settings_revision"], 1);
        assert_eq!(provenance["review_id"], p["review_id"]);
        let app = f.app();
        let config = app.config.lock().unwrap().clone();
        // Accepted chat already owns a database transaction. Destination must not read it.
        {
            let _db = app.db.lock().unwrap();
            let destination = destination(
                &app,
                &config,
                &[json!({"role":"user","content":"synthetic"})],
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(destination.provenance()).unwrap(),
                provenance
            );
        }
        let selected =
            crate::task_model::selection(&app, &serde_json::from_value(f.expected()).unwrap())
                .unwrap();
        assert_eq!(
            selected.provenance["native"]["model_provenance"],
            provenance
        );
        drop(app);
        let run = f.chat();
        f.drain().await;
        assert_eq!(
            f.run_record(&run),
            (
                THINKING_TEXT.into(),
                "completed".into(),
                0,
                provenance.clone()
            )
        );
        let request: String =
            std::fs::read_to_string(f.root.join("thinking_success.requests")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(request.trim()).unwrap()["thinking_mode"],
            "disabled"
        );
        f.unload().await;
        let next = f
            .prepare_thinking(
                "thinking_legacy",
                "model_default",
                "qwen3-default-thinking-v1",
            )
            .await;
        f.load(&next).await;
        let selected_next =
            crate::task_model::selection(&f.app(), &serde_json::from_value(f.expected()).unwrap())
                .unwrap();
        assert_ne!(selected.binding_id, selected_next.binding_id);
        assert_eq!(
            selected_next.provenance["native"]["model_provenance"]["thinking_mode"],
            "model_default"
        );
        assert_eq!(
            selected_next.provenance["native"]["model_provenance"]["settings_revision"],
            2
        );
        let default_run = f.chat();
        f.drain().await;
        assert_eq!(f.run_record(&default_run).0, THINKING_TEXT);
        let request = std::fs::read_to_string(f.root.join("thinking_legacy.requests")).unwrap();
        assert!(serde_json::from_str::<Value>(request.trim())
            .unwrap()
            .get("thinking_mode")
            .is_none());
        assert_eq!(f.run_record(&run).3, provenance);
        f.restart().await;
        assert!(f.status()["runtime"].is_null());
        assert_eq!(f.run_record(&run).3, provenance);
        let payload = f.ok(
            Method::GET,
            &format!("/api/chats/{}", run["chat_id"].as_str().unwrap()),
            Value::Null,
        );
        assert!(payload["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["run_id"] == run["run_id"]
                && message["model_provenance"] == provenance));
        let bytes = std::fs::read(f.root.join("profile.sqlite3")).unwrap();
        assert!(!bytes
            .windows(THINKING_TEXT.len())
            .any(|part| part == THINKING_TEXT.as_bytes()));
        f.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mlx_thinking_protocol_refusal_and_cancel_keep_requested_intent_and_literal_partial() {
        for mode in ["thinking_missing_ack", "thinking_cancel"] {
            let mut f = Fixture::new();
            let p = f
                .prepare_thinking(mode, "disabled", "qwen3-default-thinking-v1")
                .await;
            f.load(&p).await;
            let provenance = f.status()["loaded"]["model_provenance"].clone();
            let run = f.chat();
            if mode == "thinking_cancel" {
                let live = f
                    .app()
                    .runs
                    .lock()
                    .unwrap()
                    .get(run["run_id"].as_str().unwrap())
                    .unwrap()
                    .clone();
                let mut updates = live.tx.subscribe();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while updates.borrow_and_update().content != THINKING_TEXT {
                        updates.changed().await.unwrap();
                    }
                })
                .await
                .unwrap();
                f.ok(
                    Method::POST,
                    &format!("/api/runs/{}/cancel", run["run_id"].as_str().unwrap()),
                    Value::Null,
                );
            }
            f.drain().await;
            let expected = if mode == "thinking_cancel" {
                "cancelled"
            } else {
                "failed"
            };
            assert_eq!(
                f.run_record(&run),
                (THINKING_TEXT.into(), expected.into(), 1, provenance.clone())
            );
            assert!(!f.app().mlx.occupied());
            f.restart().await;
            assert_eq!(f.run_record(&run).3, provenance);
            f.close().await;
        }
    }
}
