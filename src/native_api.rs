//! Explicit, process-owned native text inference. Browser approval identifies a
//! prepared Rust LoadPlan; it cannot deserialize an ApprovedPlan or shell command.
use crate::*;
use native::{Cancellation, LoadPlan, Runtime};
use smith_managed_runtime as native;

#[derive(Default)]
pub struct NativeState {
    inner: Mutex<Inner>,
}
#[derive(Default)]
struct Inner {
    runtime: Option<Runtime>,
    prepared: Option<Prepared>,
    operation: Option<Operation>,
    loaded_max_tokens: u32,
}
struct Prepared {
    plan: LoadPlan,
    expected: Expected,
    runtime_revision: u64,
    preferences: Option<PreparedPreferences>,
    catalog: Option<local_catalog_api::CatalogBinding>,
}
#[derive(Clone)]
pub(crate) struct PreparedPreferences {
    pub revision: u64,
    pub intent: smith_acceleration::OptimizationIntent,
    pub record: runtime_preferences::SelectedSuccessRecord,
}
struct Operation {
    id: String,
    kind: &'static str,
    status: &'static str,
    error: Option<String>,
    config_revision: Option<String>,
    cancel_requested: bool,
    cancel: Cancellation,
}
impl NativeState {
    fn runtime(&self) -> Runtime {
        let mut state = self.inner.lock().unwrap();
        state.runtime.get_or_insert_with(Runtime::new).clone()
    }
    pub fn snapshot(&self) -> Option<native::Snapshot> {
        self.inner
            .lock()
            .unwrap()
            .runtime
            .as_ref()
            .map(Runtime::status)
    }
    pub(crate) fn revision(&self) -> u64 {
        self.runtime().status().revision
    }
    pub(crate) fn occupied(&self) -> bool {
        self.snapshot()
            .is_some_and(|s| s.pid.is_some() || s.spawned != s.reaped)
    }
    pub fn busy(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .operation
            .as_ref()
            .is_some_and(|op| op.status == "running")
    }
    fn begin(&self, kind: &'static str) -> ApiResult<(String, Cancellation)> {
        let mut state = self.inner.lock().unwrap();
        if state
            .operation
            .as_ref()
            .is_some_and(|op| op.status == "running")
        {
            return Err(busy());
        }
        let id = crate::id();
        let cancel = Cancellation::new();
        state.operation = Some(Operation {
            id: id.clone(),
            kind,
            status: "running",
            error: None,
            config_revision: None,
            cancel_requested: false,
            cancel: cancel.clone(),
        });
        Ok((id, cancel))
    }
    fn finish(&self, id: &str, result: ApiResult<()>) {
        let mut state = self.inner.lock().unwrap();
        if let Some(op) = state.operation.as_mut().filter(|op| op.id == id) {
            op.status = if result.is_ok() {
                "succeeded"
            } else if op.cancel_requested {
                "cancelled"
            } else {
                "failed"
            };
            op.error = result.err().map(|e| e.1);
        }
    }
    pub fn begin_shutdown(&self) {
        let mut state = self.inner.lock().unwrap();
        if let Some(op) = &mut state.operation {
            op.cancel_requested = true;
            op.cancel.cancel();
        }
    }
    pub async fn shutdown(&self) -> Result<(), String> {
        self.begin_shutdown();
        let runtime = self.inner.lock().unwrap().runtime.clone();
        if let Some(runtime) = runtime {
            runtime.shutdown().await.map_err(|e| failure(e).1)?;
        }
        Ok(())
    }
    fn value(&self) -> Value {
        let state = self.inner.lock().unwrap();
        json!({"runtime":state.runtime.as_ref().map(Runtime::status),"operation":state.operation.as_ref().map(|o|json!({"id":o.id,"kind":o.kind,"status":o.status,"error":o.error,"config_revision":o.config_revision})),"prepared":state.prepared.as_ref().map(|p|json!({"plan":p.plan.summary(),"runtime_revision":p.runtime_revision,"revision":p.expected.revision,"max_tokens":p.preferences.as_ref().map(|x|x.record.output_tokens).unwrap_or_else(||max_tokens(p.plan.summary().config.context_tokens))})),"streaming":false})
    }
}
fn max_tokens(context: u32) -> u32 {
    512.min(context / 2)
}
fn busy() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Finish or cancel the managed operation first.".into(),
    )
}
pub(crate) fn failure(error: native::Failure) -> ApiError {
    let status = match error {
        native::Failure::Busy
        | native::Failure::StaleRevision
        | native::Failure::StaleGeneration
        | native::Failure::NotReady
        | native::Failure::WrongModel => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    let detail=match error {
        native::Failure::ArtifactChanged=>"A selected file changed or its SHA-256 does not match. Prepare and review a new plan.",
        native::Failure::InvalidManifest=>"Check both absolute file paths, expected SHA-256 hashes, source descriptions and immutable revisions. Select a native executable and a GGUF file.",
        native::Failure::NotReady=>"The managed model is not ready. Explicitly review and load it before sending.",
        native::Failure::Cancelled=>"Managed operation cancelled. Check the current runtime state before loading again.",
        native::Failure::Cleanup=>"Owned runtime cleanup could not be confirmed. The process handle is retained; do not start another runtime.",
        native::Failure::UnsupportedPlatform=>"Managed inference is supported only on macOS and Linux by this adapter.",
        native::Failure::Busy=>"The managed runtime is busy. Wait for its operation or stop its response.",
        native::Failure::StartupTimeout=>"The owned runtime did not become ready before its deadline.",
        native::Failure::GenerationTimeout=>"The managed response exceeded its deadline; the owned runtime was stopped.",
        native::Failure::EndpointOwnership=>"The local socket did not belong to the owned runtime. No prompt was sent to that peer.",
        native::Failure::ChildExited=>"The owned runtime exited. Review its plan and explicitly load again.",
        _=>"The managed operation failed. Review its typed state and prepare a new plan if needed.",
    };
    ApiError(status, format!("{detail} ({error})"))
}
fn active(a: &App) -> bool {
    a.runs.lock().unwrap().values().any(|r| {
        let s = r.tx.borrow();
        s.status == "streaming" && s.snapshot.provider == "managed"
    })
}
pub fn ready(a: &App, c: &Config) -> ApiResult<(Runtime, native::Snapshot)> {
    if a.native.busy() {
        return Err(busy());
    }
    let snapshot = a
        .native
        .snapshot()
        .ok_or_else(|| failure(native::Failure::NotReady))?;
    if snapshot.phase != native::Phase::Ready
        || snapshot.model_id.as_deref() != Some(&c.model)
        || snapshot
            .plan_id
            .as_ref()
            .map(|id| format!("managed://{id}"))
            != Some(c.endpoint.clone())
    {
        return Err(failure(native::Failure::NotReady));
    }
    Ok((a.native.runtime(), snapshot))
}
pub fn models(a: &App, c: &Config) -> Vec<Value> {
    ready(a, c)
        .map(|(_, s)| vec![json!({"id":s.model_id.unwrap(),"managed":true,"streaming":false})])
        .unwrap_or_default()
}
pub async fn status(State(a): State<Arc<App>>) -> Json<Value> {
    let runtime = a.native.inner.lock().unwrap().runtime.clone();
    let observation = if let Some(runtime) = runtime {
        let _ = runtime.refresh_status().await;
        runtime
            .startup_observation(runtime.status().target)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let mut value = a.native.value();
    // A load/replacement may race this read. Never pair a previous child's
    // allocation report with a newer runtime snapshot.
    value["startup_observation"] = observation
        .filter(|report| {
            value["runtime"]["target"] == json!(report.identity.target)
                && value["runtime"]["plan_id"] == json!(report.identity.plan_id)
        })
        .map(|report| json!(report))
        .unwrap_or(Value::Null);
    Json(value)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileInput {
    path: String,
    sha256: String,
    source: String,
    source_revision: String,
}
impl FileInput {
    fn artifact(self) -> ApiResult<native::Artifact> {
        if self.path.len() > 4096 {
            return Err(bad("Selected path is too long."));
        }
        Ok(native::Artifact {
            path: self.path.into(),
            sha256: self.sha256,
            source: self.source,
            source_revision: self.source_revision,
        })
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Native,
    CompatibleFixture,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareInput {
    executable: FileInput,
    model: FileInput,
    model_id: String,
    execution_kind: Kind,
    context_tokens: u32,
    threads: u16,
    startup_seconds: u64,
    generation_seconds: u64,
    device: Option<String>,
    gpu_layers: Option<u32>,
    #[serde(flatten)]
    expected: Expected,
}
pub async fn prepare(
    State(a): State<Arc<App>>,
    Json(input): Json<PrepareInput>,
) -> ApiResult<Json<Value>> {
    if !(128..=32768).contains(&input.context_tokens)
        || !(1..=64).contains(&input.threads)
        || !(1..=600).contains(&input.startup_seconds)
        || !(1..=600).contains(&input.generation_seconds)
    {
        return Err(bad(
            "Use context 128–32,768, threads 1–64, and deadlines 1–600 seconds.",
        ));
    }
    let backend = match (input.device, input.gpu_layers) {
        (None, None) => native::BackendRequest::Cpu,
        (Some(name), Some(gpu_layers)) => native::BackendRequest::Device { name, gpu_layers },
        _ => {
            return Err(bad(
                "Select CPU, or supply one exact device and GPU layer count.",
            ))
        }
    };
    let manifest = native::Manifest {
        model_id: input.model_id,
        executable: input.executable.artifact()?,
        model: input.model.artifact()?,
        protocol: native::Protocol::LlamaCppUnixHttpV1,
        execution_kind: match input.execution_kind {
            Kind::Native => native::ExecutionKind::Native,
            Kind::CompatibleFixture => native::ExecutionKind::CompatibleFixture,
        },
        declares_text_chat: true,
    };
    let config = native::LoadConfig {
        backend,
        context_tokens: input.context_tokens,
        threads: input.threads,
        budgets: native::Budgets {
            startup: Duration::from_secs(input.startup_seconds),
            generation: Duration::from_secs(input.generation_seconds),
            reap: Duration::from_secs(2),
        },
    };
    let (operation, cancel, runtime_revision) = {
        let consent = a.consent.lock().unwrap();
        let c = a.config.lock().unwrap();
        validate_expected(&consent, &c, &input.expected)?;
        if c.provider != "managed" {
            return Err(bad(
                "Save Managed local model as the chosen connection before reviewing a plan.",
            ));
        }
        if active(&a) || a.acceleration.busy() || a.mlx.busy() {
            return Err(busy());
        }
        let (op, cancel) = a.native.begin("prepare")?;
        a.native.inner.lock().unwrap().prepared = None;
        (op, cancel, a.native.runtime().status().revision)
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = async {
            let plan = LoadPlan::prepare(manifest, config, &cancel)
                .await
                .map_err(failure)?;
            let consent = a.consent.lock().unwrap();
            validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
            if cancel.is_cancelled() {
                return Err(failure(native::Failure::Cancelled));
            }
            let mut state = a.native.inner.lock().unwrap();
            state.prepared = Some(Prepared {
                plan,
                expected: input.expected,
                runtime_revision,
                preferences: None,
                catalog: None,
            });
            if let Some(op) = state.operation.as_mut().filter(|op| op.id == operation) {
                op.status = "succeeded";
            }
            Ok(())
        }
        .await;
        a.native.finish(&operation, result);
    });
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadInput {
    plan_id: String,
    runtime_revision: u64,
    confirm: bool,
    #[serde(flatten)]
    expected: Expected,
}
/// An already inspected local setup plan enters the same owned-runtime gate as
/// the advanced manual flow. It is never reconstructed from a client plan JSON.
pub(crate) fn admit_selected_plan(
    a: &Arc<App>,
    consent: &Consent,
    runtime_revision: u64,
    expected: Expected,
    plan: LoadPlan,
    preferences: PreparedPreferences,
    catalog: Option<local_catalog_api::CatalogBinding>,
) -> ApiResult<Json<Value>> {
    validate_expected(consent, &a.config.lock().unwrap(), &expected)?;
    if a.native.busy()
        || a.mlx.busy()
        || a.mlx.occupied()
        || a.runs
            .lock()
            .unwrap()
            .values()
            .any(|r| r.tx.borrow().status == "streaming")
    {
        return Err(busy());
    }
    let saved =
        runtime_preferences::read(&a.db.lock().unwrap()).map_err(|e| bad(&e.to_string()))?;
    if saved.revision != preferences.revision || saved.intent != preferences.intent {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Local inference settings changed. Review a new setup plan.".into(),
        ));
    }
    if a.native.revision() != runtime_revision {
        return Err(failure(native::Failure::StaleRevision));
    }
    if let Some(binding) = &catalog {
        local_catalog_api::validate_binding_locked(a, consent, binding)?;
    }
    let plan_id = plan.summary().id.clone();
    a.native.inner.lock().unwrap().prepared = Some(Prepared {
        plan,
        expected: expected.clone(),
        runtime_revision,
        preferences: Some(preferences),
        catalog,
    });
    begin_load(
        a,
        consent,
        LoadInput {
            plan_id,
            runtime_revision,
            confirm: true,
            expected,
        },
    )
}
pub async fn load(
    State(a): State<Arc<App>>,
    Json(input): Json<LoadInput>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    if a.acceleration.busy() || a.mlx.busy() || a.mlx.occupied() {
        return Err(busy());
    }
    begin_load(&a, &consent, input)
}
fn begin_load(a: &Arc<App>, consent: &Consent, input: LoadInput) -> ApiResult<Json<Value>> {
    let (operation, cancel, approved, runtime, preferences, catalog) = {
        validate_expected(consent, &a.config.lock().unwrap(), &input.expected)?;
        if !input.confirm {
            return Err(bad(
                "Review the complete plan and explicitly confirm loading these selected files.",
            ));
        }
        if active(a) || a.native.busy() {
            return Err(busy());
        }
        let mut state = a.native.inner.lock().unwrap();
        let prepared = state
            .prepared
            .as_ref()
            .ok_or_else(|| bad("Prepare and review a load plan first."))?;
        if prepared.expected.revision != input.expected.revision
            || prepared.plan.summary().id != input.plan_id
            || prepared.runtime_revision != input.runtime_revision
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "This reviewed plan is stale. Prepare and review it again.".into(),
            ));
        }
        let catalog = prepared.catalog.clone();
        if let Some(binding) = &catalog {
            local_catalog_api::validate_binding_locked(a, consent, binding)?;
        }
        let approved = prepared
            .plan
            .clone()
            .approve(&input.plan_id)
            .map_err(failure)?;
        let preferences = prepared.preferences.clone();
        if let Some(preferences) = &preferences {
            let saved = runtime_preferences::read(&a.db.lock().unwrap())
                .map_err(|e| bad(&e.to_string()))?;
            if saved.revision != preferences.revision || saved.intent != preferences.intent {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "Local inference settings changed. Review a new setup plan.".into(),
                ));
            }
        }
        state.prepared = None;
        drop(state);
        let (operation, cancel) = a.native.begin("load")?;
        (
            operation,
            cancel,
            approved,
            a.native.runtime(),
            preferences,
            catalog,
        )
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let before = runtime.status();
        let response_tokens = preferences
            .as_ref()
            .map(|p| p.record.output_tokens)
            .unwrap_or_else(|| max_tokens(approved.summary().config.context_tokens));
        let mut newly_loaded = None;
        let mut result = async {
            let snapshot = runtime
                .load(input.runtime_revision, approved, cancel.clone())
                .await
                .map_err(failure)?;
            if snapshot.target != before.target {
                newly_loaded = Some((snapshot.target, snapshot.model_id.clone().unwrap()));
            }
            let mut consent = a.consent.lock().unwrap();
            if let Some(binding) = &catalog {
                local_catalog_api::validate_binding_locked(&a, &consent, binding)?;
            }
            // Cancellation and the successful configuration commit share one lock.
            // After this commit, cancel must observe an already-completed operation.
            if cancel.is_cancelled() {
                return Err(failure(native::Failure::Cancelled));
            }
            let mut c = a.config.lock().unwrap();
            validate_expected(&consent, &c, &input.expected)?;
            let next = Config {
                provider: "managed".into(),
                endpoint: format!("managed://{}", snapshot.plan_id.as_deref().unwrap()),
                model: snapshot.model_id.clone().unwrap(),
            };
            {
                let mut database = a.db.lock().unwrap();
                let transaction = database.transaction()?;
                transaction.execute(
                    "INSERT OR REPLACE INTO settings VALUES('config',?1)",
                    [serde_json::to_string(&next).unwrap()],
                )?;
                if let Some(preferences) = &preferences {
                    runtime_preferences::commit_selected_success(
                        &transaction,
                        preferences.revision,
                        &preferences.intent,
                        &preferences.record,
                    )
                    .map_err(|e| bad(&e.to_string()))?;
                }
                transaction.commit()?;
            }
            *c = next;
            consent.advance();
            {
                let mut state = a.native.inner.lock().unwrap();
                state.loaded_max_tokens = response_tokens;
                if let Some(op) = state.operation.as_mut() {
                    op.config_revision = Some(consent.token());
                    op.status = "succeeded";
                }
            }
            Ok(())
        }
        .await;
        // A successfully spawned model must not survive a failed configuration commit.
        if result.is_err() {
            if let Some((target, model)) = newly_loaded {
                let s = runtime.status();
                if s.phase == native::Phase::Ready && s.target == target {
                    if let Err(cleanup) = runtime.unload(target, model).await {
                        result = Err(failure(cleanup));
                    }
                }
            }
        }
        a.native.finish(&operation, result);
    });
    Ok(Json(response))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelInput {
    operation_id: String,
    #[serde(flatten)]
    expected: Expected,
}
pub async fn cancel(
    State(a): State<Arc<App>>,
    Json(input): Json<CancelInput>,
) -> ApiResult<Json<Value>> {
    let consent = a.consent.lock().unwrap();
    validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
    let mut state = a.native.inner.lock().unwrap();
    let op = state
        .operation
        .as_mut()
        .filter(|o| o.id == input.operation_id && o.status == "running")
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                "That managed operation is no longer active.".into(),
            )
        })?;
    if op.kind == "unload" {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Owned runtime cleanup is already in progress and must finish.".into(),
        ));
    }
    op.cancel_requested = true;
    op.cancel.cancel();
    Ok(Json(
        json!({"ok":true,"message":"Cancellation requested. Waiting for owned process cleanup."}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnloadInput {
    owner: String,
    generation: u64,
    runtime_revision: u64,
    model_id: String,
    #[serde(flatten)]
    expected: Expected,
}
pub async fn unload(
    State(a): State<Arc<App>>,
    Json(input): Json<UnloadInput>,
) -> ApiResult<Json<Value>> {
    let (operation, runtime, target) = {
        let consent = a.consent.lock().unwrap();
        validate_expected(&consent, &a.config.lock().unwrap(), &input.expected)?;
        if active(&a) || a.native.busy() || a.acceleration.busy() || a.mlx.busy() {
            return Err(busy());
        }
        let runtime = a.native.runtime();
        let s = runtime.status();
        if s.target.owner.to_string() != input.owner
            || s.target.generation != input.generation
            || s.revision != input.runtime_revision
            || s.model_id.as_deref() != Some(&input.model_id)
        {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "The managed runtime changed. Refresh its state before unloading.".into(),
            ));
        }
        let (op, _) = a.native.begin("unload")?;
        (op, runtime, s.target)
    };
    let response = json!({"operation_id":operation});
    let task_app = a.clone();
    a.spawn_task(async move {
        let a = task_app;
        let result = runtime
            .unload(target, input.model_id)
            .await
            .map(|_| ())
            .map_err(failure);
        a.native.finish(&operation, result);
    });
    Ok(Json(response))
}

pub struct Destination {
    runtime: Runtime,
    snapshot: native::Snapshot,
    max_tokens: u32,
}
pub fn destination(a: &App, c: &Config, history: &[Value]) -> ApiResult<Destination> {
    let (runtime, snapshot) = ready(a, c)?;
    let max_tokens = a.native.inner.lock().unwrap().loaded_max_tokens.max(1);
    let body = json!({"model":c.model,"messages":history.iter().map(|m|json!({"role":m["role"],"content":m["content"]})).collect::<Vec<_>>(),"max_tokens":max_tokens,"stream":false});
    if history.is_empty()
        || history.len() > 256
        || serde_json::to_vec(&body).unwrap().len() > 128 * 1024
    {
        return Err(bad("Managed text requests support at most 256 messages and 128 KiB of serialized context. Start a shorter conversation."));
    }
    Ok(Destination {
        runtime,
        snapshot,
        max_tokens,
    })
}
pub async fn generate(
    a: &App,
    run: &Run,
    destination: Destination,
    history: Vec<Value>,
) -> Result<streaming::Outcome, String> {
    let cancel = Cancellation::new();
    let request = native::TextRequest {
        messages: history
            .into_iter()
            .map(|m| native::Message {
                role: match m["role"].as_str() {
                    Some("system") => native::Role::System,
                    Some("assistant") => native::Role::Assistant,
                    _ => native::Role::User,
                },
                content: m["content"].as_str().unwrap_or("").into(),
            })
            .collect(),
        max_tokens: destination.max_tokens,
    };
    let mut starting = Box::pin(destination.runtime.start_generation(
        destination.snapshot.target,
        native::Capability::TextChat,
        request,
        cancel.clone(),
    ));
    let admitted = tokio::select! {biased;_=run.cancel.notified()=>{cancel.cancel();starting.await},result=&mut starting=>result};
    let result = match admitted {
        Ok(mut generation) => {
            tokio::select! {biased;_=run.cancel.notified()=>{generation.cancel();generation.wait().await},result=generation.wait()=>result}
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(completion) => {
            append(a, run, &completion.text)?;
            Ok(streaming::Outcome{status:if completion.finish_reason=="length"{"limited"}else{"completed"}.into(),finish_reason:completion.finish_reason,notice:Some("Managed local text response returned as one complete result. Observed compute backend is unknown.".into()),transport_incomplete:false})
        }
        Err(native::Failure::Cancelled) => Err("cancelled".into()),
        Err(e) => Err(failure(e).1),
    }
}

/// Task inference uses the same owned supervisor and cancellation/reap contract,
/// but persists its result through the task engine rather than a chat message.
pub(crate) async fn task_generate(
    destination: Destination,
    history: Vec<Value>,
    cancel: Cancellation,
) -> Result<native::Completion, native::Failure> {
    let request = native::TextRequest {
        messages: history
            .into_iter()
            .map(|m| native::Message {
                role: match m["role"].as_str() {
                    Some("system") => native::Role::System,
                    Some("assistant") => native::Role::Assistant,
                    _ => native::Role::User,
                },
                content: m["content"].as_str().unwrap_or("").to_owned(),
            })
            .collect(),
        max_tokens: destination.max_tokens,
    };
    let mut generation = destination
        .runtime
        .start_task_generation(destination.snapshot.target, request, cancel)
        .await?;
    generation.wait().await
}
