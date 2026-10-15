//! Passive encrypted local selections. Historical records never grant loading
//! authority; only a fresh, retained handoff can enter an existing setup flow.
use crate::*;
use rusqlite::TransactionBehavior;
use sha2::{Digest, Sha256};
use smith_local_catalog as catalog;
use std::{collections::VecDeque, time::Instant};

const SETTINGS_KEY: &str = "local_model_catalog";
const MAX_STORED: usize = 2 * 1024 * 1024;
const MAX_RECEIPTS: usize = 256;
const MAX_RECEIPT: usize = 8192;
const MAX_OPERATIONS: usize = 32;
const REVIEW_LIFETIME: Duration = Duration::from_secs(300);

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn hash(value: &impl Serialize) -> ApiResult<String> {
    Ok(digest(&serde_json::to_vec(value).map_err(|_| malformed())?))
}
fn malformed() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Saved local catalog data is malformed or unsupported; it was not reset.".into(),
    )
}
fn conflict() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "The local catalog or reviewed context changed. Refresh before continuing.".into(),
    )
}
fn closed() -> ApiError {
    ApiError(
        StatusCode::CONFLICT,
        "Local catalog work is closing or still draining.".into(),
    )
}
fn limited() -> ApiError {
    ApiError(StatusCode::CONFLICT, "The bounded local catalog or its retained request receipts are full. No records were discarded.".into())
}
fn catalog_error(error: catalog::Error) -> ApiError {
    let status = match error {
        catalog::Error::MissingEntry => StatusCode::NOT_FOUND,
        catalog::Error::Conflict
        | catalog::Error::StaleReview
        | catalog::Error::WrongSelection
        | catalog::Error::EntryTrashed
        | catalog::Error::InvalidRecord
        | catalog::Error::UnsupportedVersion => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError(status, error.to_string())
}
fn uuid(value: &str) -> ApiResult<()> {
    if !uuid::Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value) {
        return Err(bad("Use the exact request or operation UUID."));
    }
    Ok(())
}
fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn present_option<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}

/// Fingerprint identifies the exact encrypted setting's decoded text bytes.
/// None means no row existed, and is distinct from a stored empty catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogVersion {
    pub revision: u64,
    #[serde(deserialize_with = "present_option")]
    pub sha256: Option<String>,
}
impl CatalogVersion {
    fn validate(&self) -> ApiResult<()> {
        if self.revision > catalog::MAX_REVISION
            || self.sha256.as_ref().is_some_and(|s| !sha(s))
            || (self.sha256.is_none() && self.revision != 0)
        {
            return Err(bad(
                "Use the exact catalog revision and fingerprint returned by the app.",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    request_id: String,
    request_sha256: String,
    result: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format_version: u8,
    /// Preserve the exact inner catalog bytes for its own storage-base check.
    catalog_json: String,
    receipts: Vec<Receipt>,
}
struct Snapshot {
    raw: Option<String>,
    catalog: catalog::Catalog,
    receipts: Vec<Receipt>,
}
impl Snapshot {
    fn version(&self) -> CatalogVersion {
        CatalogVersion {
            revision: self.catalog.revision(),
            sha256: self.raw.as_ref().map(|r| digest(r.as_bytes())),
        }
    }
    fn require(&self, version: &CatalogVersion) -> ApiResult<()> {
        version.validate()?;
        if self.version() != *version {
            return Err(conflict());
        }
        Ok(())
    }
    fn receipt(&self, request: &str, request_hash: &str) -> ApiResult<Option<Value>> {
        match self.receipts.iter().find(|r| r.request_id == request) {
            Some(receipt) if receipt.request_sha256 == request_hash => {
                Ok(Some(receipt.result.clone()))
            }
            Some(_) => Err(conflict()),
            None => Ok(None),
        }
    }
    fn require_capacity(&self) -> ApiResult<()> {
        if self.receipts.len() >= MAX_RECEIPTS {
            return Err(limited());
        }
        Ok(())
    }
}
fn read(db: &Connection) -> ApiResult<Snapshot> {
    let raw: Option<Option<String>> = db.query_row(
        "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB))<=?2 THEN value END FROM settings WHERE key=?1",
        params![SETTINGS_KEY, MAX_STORED], |r| r.get(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(Snapshot {
            raw: None,
            catalog: catalog::Catalog::empty(),
            receipts: vec![],
        });
    };
    let raw = raw.ok_or_else(malformed)?;
    let stored: Stored = serde_json::from_str(&raw).map_err(|_| malformed())?;
    if stored.format_version != 1 || stored.receipts.len() > MAX_RECEIPTS {
        return Err(malformed());
    }
    let catalog =
        catalog::Catalog::decode(stored.catalog_json.as_bytes()).map_err(catalog_error)?;
    let mut ids = std::collections::BTreeSet::new();
    for receipt in &stored.receipts {
        if uuid(&receipt.request_id).is_err()
            || !sha(&receipt.request_sha256)
            || !ids.insert(&receipt.request_id)
            || !receipt.result.is_object()
            || receipt.result.to_string().len() > MAX_RECEIPT
            || receipt.result["request_id"] != receipt.request_id
            || receipt.result["catalog_revision"]
                .as_u64()
                .is_none_or(|r| r == 0 || r > catalog.revision())
        {
            return Err(malformed());
        }
    }
    Ok(Snapshot {
        raw: Some(raw),
        catalog,
        receipts: stored.receipts,
    })
}
fn save(
    tx: &rusqlite::Transaction<'_>,
    before: Snapshot,
    next: catalog::Catalog,
    receipt: Receipt,
) -> ApiResult<CatalogVersion> {
    before.require_capacity()?;
    if next.storage_base_revision() != before.catalog.storage_base_revision()
        || next.storage_base_sha256() != before.catalog.storage_base_sha256()
        || next.revision()
            != before
                .catalog
                .revision()
                .checked_add(1)
                .ok_or_else(limited)?
    {
        return Err(conflict());
    }
    if receipt.result.to_string().len() > MAX_RECEIPT {
        return Err(limited());
    }
    let mut receipts = before.receipts;
    receipts.push(receipt);
    let catalog_json =
        String::from_utf8(next.encode().map_err(catalog_error)?).map_err(|_| malformed())?;
    let value = serde_json::to_string(&Stored {
        format_version: 1,
        catalog_json,
        receipts,
    })
    .map_err(|_| malformed())?;
    if value.len() > MAX_STORED {
        return Err(limited());
    }
    let changed = match before.raw {
        Some(raw) => tx.execute("UPDATE settings SET value=?2 WHERE key=?1 AND typeof(value)='text' AND value=?3 COLLATE BINARY", params![SETTINGS_KEY, value, raw])?,
        None => tx.execute("INSERT INTO settings(key,value) SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM settings WHERE key=?1)", params![SETTINGS_KEY, value])?,
    };
    if changed != 1 {
        return Err(conflict());
    }
    // Decode the committed candidate as well: a successful write never leaves
    // its in-memory catalog carrying the old storage-base fingerprint.
    let checked = read(tx)?;
    Ok(checked.version())
}
fn receipt_response(receipt: Value, replayed: bool, version: CatalogVersion) -> Value {
    json!({"receipt":receipt,"replayed":replayed,"catalog":version,"execution":"none","model_files_changed":false})
}
fn summary(catalog: &catalog::Catalog, id: &str) -> ApiResult<Value> {
    catalog
        .list()
        .into_iter()
        .chain(catalog.list_trash())
        .find(|e| e.id == id)
        .map(|entry| json!(entry))
        .ok_or_else(|| catalog_error(catalog::Error::MissingEntry))
}

#[derive(Clone, Serialize)]
struct Operation {
    id: String,
    kind: &'static str,
    status: &'static str,
    request_id: Option<String>,
    request_sha256: Option<String>,
    submitted_catalog: CatalogVersion,
    entry_id: Option<String>,
    cancel_requested: bool,
    worker_drained: bool,
    result: Option<Value>,
    error: Option<String>,
    #[serde(skip)]
    control: catalog::ReadControl,
}
#[derive(Default)]
struct Inner {
    closing: bool,
    operations: VecDeque<Operation>,
    prepared: Option<Arc<PreparedCatalog>>,
}
#[derive(Default)]
struct Shared {
    /// Serializes cancellation/revocation with durable publication. Never held
    /// across file reads, subprocess work or await points.
    publication: Mutex<()>,
    inner: Mutex<Inner>,
}
#[derive(Clone, Default)]
pub struct CatalogState {
    shared: Arc<Shared>,
}
enum WorkFailure {
    Api(ApiError),
    Cancelled,
}
impl From<ApiError> for WorkFailure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}
impl From<catalog::Error> for WorkFailure {
    fn from(error: catalog::Error) -> Self {
        if error == catalog::Error::Cancelled {
            Self::Cancelled
        } else {
            Self::Api(catalog_error(error))
        }
    }
}
struct Lease {
    shared: Arc<Shared>,
    id: String,
    armed: bool,
}
impl Lease {
    fn finish(mut self, outcome: Result<Value, WorkFailure>) {
        let _publication = self.shared.publication.lock().unwrap();
        finish_locked(&self.shared, &self.id, outcome);
        self.armed = false;
    }
    /// The callback and terminal status are published under the same gate as
    /// cancel/shutdown. A committed mutation cannot then be reported cancelled.
    fn publish(mut self, action: impl FnOnce() -> ApiResult<Value>) {
        let _publication = self.shared.publication.lock().unwrap();
        let result = {
            let inner = self.shared.inner.lock().unwrap();
            if inner.closing {
                Err(WorkFailure::Cancelled)
            } else {
                inner
                    .operations
                    .iter()
                    .find(|o| o.id == self.id)
                    .ok_or_else(|| WorkFailure::Api(closed()))
                    .and_then(|o| o.control.check().map_err(Into::into))
            }
        }
        .and_then(|()| action().map_err(Into::into));
        finish_locked(&self.shared, &self.id, result);
        self.armed = false;
    }
}
fn finish_locked(shared: &Shared, id: &str, outcome: Result<Value, WorkFailure>) {
    let mut inner = shared.inner.lock().unwrap();
    if let Some(op) = inner
        .operations
        .iter_mut()
        .find(|o| o.id == id && o.status == "running")
    {
        op.worker_drained = true;
        match outcome {
            Ok(result) => {
                op.status = "succeeded";
                op.result = Some(result);
            }
            Err(WorkFailure::Cancelled) => {
                op.status = "cancelled";
                op.error =
                    Some("Catalog file validation cancelled; no result was published.".into());
            }
            Err(WorkFailure::Api(error)) => {
                op.status = "failed";
                op.error = Some(error.1);
            }
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.armed {
            let Ok(_publication) = self.shared.publication.lock() else {
                return;
            };
            let Ok(mut inner) = self.shared.inner.lock() else {
                return;
            };
            if let Some(op) = inner
                .operations
                .iter_mut()
                .find(|o| o.id == self.id && o.status == "running")
            {
                op.control.cancel();
                op.cancel_requested = true;
                op.status = "failed";
                // An abandoned future is not proof its blocking child drained.
                // Keep the profile occupied rather than claim cleanup success.
                op.error=Some("Catalog worker ownership ended without a drain receipt. The profile remains open; inspect the durable request receipt before recovery.".into());
            }
        }
    }
}
impl CatalogState {
    pub fn begin_shutdown(&self) {
        let _publication = self.shared.publication.lock().unwrap();
        let mut inner = self.shared.inner.lock().unwrap();
        inner.closing = true;
        inner.prepared = None;
        for op in inner
            .operations
            .iter_mut()
            .filter(|o| o.status == "running")
        {
            op.cancel_requested = true;
            op.control.cancel();
        }
    }
    pub fn shutdown_result(&self) -> Result<(), String> {
        if self
            .shared
            .inner
            .lock()
            .unwrap()
            .operations
            .iter()
            .any(|o| !o.worker_drained)
        {
            Err("Catalog file workers have not drained; the encrypted profile remains open.".into())
        } else {
            Ok(())
        }
    }
    fn open(&self) -> ApiResult<()> {
        if self.shared.inner.lock().unwrap().closing {
            Err(closed())
        } else {
            Ok(())
        }
    }
    fn begin(
        &self,
        kind: &'static str,
        version: CatalogVersion,
        entry: Option<String>,
        request: Option<(String, String)>,
    ) -> ApiResult<(Lease, catalog::ReadControl, Value)> {
        self.open()?;
        let mut inner = self.shared.inner.lock().unwrap();
        if inner.operations.iter().any(|o| !o.worker_drained) {
            return Err(closed());
        }
        while inner.operations.len() >= MAX_OPERATIONS {
            inner.operations.pop_front();
        }
        let id = crate::id();
        let control = catalog::ReadControl::new(REVIEW_LIFETIME).map_err(catalog_error)?;
        let (request_id, request_sha256) =
            request.map_or((None, None), |(id, hash)| (Some(id), Some(hash)));
        let op = Operation {
            id: id.clone(),
            kind,
            status: "running",
            request_id,
            request_sha256,
            submitted_catalog: version,
            entry_id: entry,
            cancel_requested: false,
            worker_drained: false,
            result: None,
            error: None,
            control: control.clone(),
        };
        let response = json!({"operation_id":id,"operation":op});
        inner.operations.push_back(op);
        Ok((
            Lease {
                shared: self.shared.clone(),
                id,
                armed: true,
            },
            control,
            response,
        ))
    }
    fn active_request(&self, id: &str, hash: &str) -> ApiResult<Option<Value>> {
        let inner = self.shared.inner.lock().unwrap();
        match inner
            .operations
            .iter()
            .find(|o| o.request_id.as_deref() == Some(id) && o.status == "running")
        {
            Some(op) if op.request_sha256.as_deref() == Some(hash) => {
                Ok(Some(json!({"operation_id":op.id,"operation":op})))
            }
            Some(_) => Err(conflict()),
            None => Ok(None),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CatalogBinding {
    review: Arc<catalog::SelectionReview>,
    version: CatalogVersion,
    expected: Expected,
    expires: Instant,
    manifest: catalog::ManifestRecord,
}
/// Getter-only host object, never reconstructed from a catalog/HTTP JSON body.
pub(crate) struct PreparedCatalog {
    selected: catalog::PreparedSelection,
    binding: CatalogBinding,
}
impl PreparedCatalog {
    pub(crate) fn entry(&self) -> &catalog::EntryRecord {
        self.selected.entry()
    }
    pub(crate) fn binding(&self) -> CatalogBinding {
        self.binding.clone()
    }
}
fn context_locked(app: &App, consent: &Consent) -> ApiResult<catalog::PreparationContext> {
    Ok(catalog::PreparationContext {
        session_id: app.profile_session().into(),
        config_revision: consent.token(),
        inventory_generation: acceleration_api::catalog_inventory_generation(app)?,
    })
}
/// Call while holding the existing consent gate; it serializes connection and
/// hardware observation replacement with the host adapter's publication.
pub(crate) fn validate_binding_locked(
    app: &App,
    consent: &Consent,
    binding: &CatalogBinding,
) -> ApiResult<()> {
    app.local_catalog.open()?;
    if Instant::now() >= binding.expires {
        return Err(conflict());
    }
    validate_expected(consent, &app.config.lock().unwrap(), &binding.expected)?;
    let current = context_locked(app, consent)?;
    let saved = read(&app.db.lock().unwrap())?;
    saved.require(&binding.version)?;
    saved
        .catalog
        .check_review(&binding.review, &current)
        .map_err(catalog_error)
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/local-catalog", get(list))
        .route("/api/local-catalog/entries", post(register))
        .route(
            "/api/local-catalog/entries/{id}",
            get(details).put(replace).delete(trash),
        )
        .route("/api/local-catalog/entries/{id}/rename", post(rename))
        .route("/api/local-catalog/entries/{id}/restore", post(restore))
        .route("/api/local-catalog/select", post(select))
        .route("/api/local-catalog/prepare", post(prepare))
        .route("/api/local-catalog/setup", post(setup))
        .route("/api/local-catalog/operations/{id}", get(operation))
        .route("/api/local-catalog/operations/{id}/cancel", post(cancel))
        .route("/api/local-catalog/requests/{id}", get(request_receipt))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    trashed: bool,
}
async fn list(
    State(app): State<Arc<App>>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let saved = read(&app.db.lock().unwrap())?;
    let inner = app.local_catalog.shared.inner.lock().unwrap();
    Ok(Json(
        json!({"catalog":saved.version(),"entries":if query.trashed {saved.catalog.list_trash()} else {saved.catalog.list()},"selected_for_review":saved.catalog.record().selected_for_review,"operations":inner.operations,"prepared":inner.prepared.as_ref().map(|p|p.selected.summary()),"limits":{"entries_including_trash":catalog::MAX_ENTRIES,"receipts":MAX_RECEIPTS,"stored_bytes":MAX_STORED},"inspection":"stored_metadata_only","automatic_load":false,"hints_applied":false}),
    ))
}
async fn details(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let saved = read(&app.db.lock().unwrap())?;
    Ok(Json(
        json!({"catalog":saved.version(),"entry":saved.catalog.details(&id).map_err(catalog_error)?,"inspection":"stored_metadata_only","file_availability":"not_checked"}),
    ))
}
async fn request_receipt(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    let saved = read(&app.db.lock().unwrap())?;
    let receipt = saved.receipts.iter().find(|r| r.request_id == id);
    let inner = app.local_catalog.shared.inner.lock().unwrap();
    let op = inner
        .operations
        .iter()
        .rev()
        .find(|o| o.request_id.as_deref() == Some(&id));
    if receipt.is_none() && op.is_none() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "No catalog receipt or retained operation has this request ID.".into(),
        ));
    }
    Ok(Json(
        json!({"catalog":saved.version(),"receipt":receipt,"operation":op,"missing_receipt_is_not_success":true}),
    ))
}
async fn operation(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    let inner = app.local_catalog.shared.inner.lock().unwrap();
    let op=inner.operations.iter().find(|o|o.id==id).ok_or_else(||ApiError(StatusCode::NOT_FOUND,"Catalog operation is not retained in this session. Inspect its durable request receipt.".into()))?;
    Ok(Json(json!({"operation":op})))
}
async fn cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    uuid(&id)?;
    let _publication = app.local_catalog.shared.publication.lock().unwrap();
    let mut inner = app.local_catalog.shared.inner.lock().unwrap();
    let op = inner
        .operations
        .iter_mut()
        .find(|o| o.id == id)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "Catalog operation is not retained.".into(),
            )
        })?;
    if op.status == "running" {
        op.cancel_requested = true;
        op.control.cancel();
    }
    Ok(Json(
        json!({"operation":op,"message":"An active read remains owned until its worker drains."}),
    ))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureInput {
    request_id: String,
    catalog: CatalogVersion,
    label: String,
    selection: catalog::CaptureRequest,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceInput {
    request_id: String,
    catalog: CatalogVersion,
    entry_revision: u64,
    label: String,
    selection: catalog::CaptureRequest,
}
async fn register(
    State(app): State<Arc<App>>,
    Json(input): Json<CaptureInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let hash = hash(&json!({"operation":"register_v1","input":input}))?;
    capture_start(
        app,
        input.request_id,
        hash,
        input.catalog,
        None,
        input.label,
        input.selection,
    )
    .await
}
async fn replace(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<ReplaceInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let hash = hash(&json!({"operation":"replace_v1","entry_id":id,"input":input}))?;
    capture_start(
        app,
        input.request_id,
        hash,
        input.catalog,
        Some((id, input.entry_revision)),
        input.label,
        input.selection,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn capture_start(
    app: Arc<App>,
    request_id: String,
    request_hash: String,
    version: CatalogVersion,
    entry: Option<(String, u64)>,
    label: String,
    selection: catalog::CaptureRequest,
) -> ApiResult<(StatusCode, Json<Value>)> {
    uuid(&request_id)?;
    version.validate()?;
    let (lease, control, response) = {
        let _consent = app.consent.lock().unwrap();
        let _publication = app.local_catalog.shared.publication.lock().unwrap();
        app.local_catalog.open()?;
        let saved = read(&app.db.lock().unwrap())?;
        if let Some(result) = saved.receipt(&request_id, &request_hash)? {
            return Ok((
                StatusCode::OK,
                Json(receipt_response(result, true, saved.version())),
            ));
        }
        if let Some(active) = app
            .local_catalog
            .active_request(&request_id, &request_hash)?
        {
            return Ok((StatusCode::ACCEPTED, Json(active)));
        }
        saved.require(&version)?;
        saved.require_capacity()?;
        if label.trim().is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
            return Err(bad(
                "Use a catalog name of 1–256 bytes without control characters.",
            ));
        }
        if entry.is_none() && saved.catalog.record().entries.len() >= catalog::MAX_ENTRIES {
            return Err(limited());
        }
        if let Some((id, revision)) = &entry {
            let existing = saved.catalog.details(id).map_err(catalog_error)?;
            if existing.revision != *revision || existing.trashed {
                return Err(conflict());
            }
        }
        app.local_catalog.begin(
            if entry.is_some() {
                "replace"
            } else {
                "register"
            },
            version.clone(),
            entry.as_ref().map(|e| e.0.clone()),
            Some((request_id.clone(), request_hash.clone())),
        )?
    };
    let worker_app = app.clone();
    app.spawn_task(async move {
        let worker_control=control.clone();
        let captured=tokio::task::spawn_blocking(move||catalog::capture(selection,&worker_control)).await;
        match captured {
            Ok(Ok(assets))=>{
                let _consent=worker_app.consent.lock().unwrap();
                lease.publish(||{
                let mut db=worker_app.db.lock().unwrap();
                let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let saved=read(&tx)?;
                if let Some(result)=saved.receipt(&request_id,&request_hash)? { return Ok(receipt_response(result,true,saved.version())); }
                saved.require(&version)?;
                let next=match &entry {
                    Some((id,revision))=>saved.catalog.replace(version.revision,id,*revision,label,assets),
                    None=>saved.catalog.insert(version.revision,label,assets),
                }.map_err(catalog_error)?;
                let id=entry.as_ref().map(|e|e.0.clone()).unwrap_or_else(||next.record().entries.last().unwrap().id.clone());
                let result=json!({"request_id":request_id,"catalog_revision":next.revision(),"entry":summary(&next,&id)?,"operation":if entry.is_some(){"replaced"}else{"registered"}});
                let version=save(&tx,saved,next,Receipt{request_id,request_sha256:request_hash,result:result.clone()})?;
                tx.commit()?;
                worker_app.local_catalog.shared.inner.lock().unwrap().prepared=None;
                Ok(receipt_response(result,false,version))
            }); },
            Ok(Err(error))=>lease.finish(Err(error.into())),
            Err(_)=>lease.finish(Err(WorkFailure::Api(ApiError(StatusCode::INTERNAL_SERVER_ERROR,"Catalog inspection worker failed; no result was published.".into())))),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationInput {
    request_id: String,
    catalog: CatalogVersion,
    entry_revision: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameInput {
    request_id: String,
    catalog: CatalogVersion,
    entry_revision: u64,
    label: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectInput {
    request_id: String,
    catalog: CatalogVersion,
    entry_id: String,
    entry_revision: u64,
}
fn mutate(
    app: &App,
    id: &str,
    input: MutationInput,
    operation: &str,
    label: Option<String>,
) -> ApiResult<Json<Value>> {
    uuid(&input.request_id)?;
    input.catalog.validate()?;
    let request_hash =
        hash(&json!({"operation":operation,"entry_id":id,"input":input,"label":label}))?;
    let _consent = app.consent.lock().unwrap();
    let _publication = app.local_catalog.shared.publication.lock().unwrap();
    app.local_catalog.open()?;
    let mut db = app.db.lock().unwrap();
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let saved = read(&tx)?;
    if let Some(result) = saved.receipt(&input.request_id, &request_hash)? {
        return Ok(Json(receipt_response(result, true, saved.version())));
    }
    saved.require(&input.catalog)?;
    let next = match operation {
        "trash_v1" => saved
            .catalog
            .trash(input.catalog.revision, id, input.entry_revision),
        "restore_v1" => saved
            .catalog
            .restore(input.catalog.revision, id, input.entry_revision),
        "select_v1" => {
            saved
                .catalog
                .select_for_review(input.catalog.revision, id, input.entry_revision)
        }
        "rename_v1" => saved.catalog.rename(
            input.catalog.revision,
            id,
            input.entry_revision,
            label.ok_or_else(|| bad("Name is required."))?,
        ),
        _ => return Err(bad("Unknown catalog mutation.")),
    }
    .map_err(catalog_error)?;
    let result = json!({"request_id":input.request_id,"catalog_revision":next.revision(),"entry":summary(&next,id)?,"operation":operation});
    let version = save(
        &tx,
        saved,
        next,
        Receipt {
            request_id: input.request_id,
            request_sha256: request_hash,
            result: result.clone(),
        },
    )?;
    tx.commit()?;
    app.local_catalog.shared.inner.lock().unwrap().prepared = None;
    Ok(Json(receipt_response(result, false, version)))
}
async fn trash(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<MutationInput>,
) -> ApiResult<Json<Value>> {
    mutate(&app, &id, input, "trash_v1", None)
}
async fn restore(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<MutationInput>,
) -> ApiResult<Json<Value>> {
    mutate(&app, &id, input, "restore_v1", None)
}
async fn rename(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(input): Json<RenameInput>,
) -> ApiResult<Json<Value>> {
    mutate(
        &app,
        &id,
        MutationInput {
            request_id: input.request_id,
            catalog: input.catalog,
            entry_revision: input.entry_revision,
        },
        "rename_v1",
        Some(input.label),
    )
}
async fn select(
    State(app): State<Arc<App>>,
    Json(input): Json<SelectInput>,
) -> ApiResult<Json<Value>> {
    mutate(
        &app,
        &input.entry_id,
        MutationInput {
            request_id: input.request_id,
            catalog: input.catalog,
            entry_revision: input.entry_revision,
        },
        "select_v1",
        None,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareInput {
    catalog: CatalogVersion,
    entry_id: String,
    entry_revision: u64,
    expected: Expected,
}
async fn prepare(
    State(app): State<Arc<App>>,
    Json(input): Json<PrepareInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (lease, control, response, saved, review, binding, context) = {
        let consent = app.consent.lock().unwrap();
        let _publication = app.local_catalog.shared.publication.lock().unwrap();
        app.local_catalog.open()?;
        validate_expected(&consent, &app.config.lock().unwrap(), &input.expected)?;
        let context = context_locked(&app, &consent)?;
        let saved = read(&app.db.lock().unwrap())?;
        saved.require(&input.catalog)?;
        let review = Arc::new(
            saved
                .catalog
                .review(&input.entry_id, input.entry_revision, context.clone())
                .map_err(catalog_error)?,
        );
        let binding = CatalogBinding {
            review: review.clone(),
            version: input.catalog.clone(),
            expected: input.expected,
            expires: Instant::now() + REVIEW_LIFETIME,
            manifest: saved
                .catalog
                .details(&input.entry_id)
                .map_err(catalog_error)?
                .manifest
                .clone(),
        };
        let (lease, control, response) =
            app.local_catalog
                .begin("prepare", input.catalog, Some(input.entry_id), None)?;
        app.local_catalog.shared.inner.lock().unwrap().prepared = None;
        (
            lease,
            control,
            response,
            saved.catalog,
            review,
            binding,
            context,
        )
    };
    let worker_app = app.clone();
    app.spawn_task(async move {
        let reader=control.clone();
        let result=tokio::task::spawn_blocking(move||saved.prepare(&review,&context,&reader)).await;
        match result {
            Ok(Ok(selected))=>{
                let consent=worker_app.consent.lock().unwrap();
                lease.publish(||{
                    validate_binding_locked(&worker_app,&consent,&binding)?;
                    let result=json!({"prepared":selected.summary(),"entry":selected.entry().id,"execution":"none","qualification":"not_established","hints_applied":false});
                    worker_app.local_catalog.shared.inner.lock().unwrap().prepared=Some(Arc::new(PreparedCatalog{selected,binding}));
                    Ok(result)
                });
            }
            Ok(Err(error))=>lease.finish(Err(error.into())),
            Err(_)=>lease.finish(Err(WorkFailure::Api(ApiError(StatusCode::INTERNAL_SERVER_ERROR,"Catalog validation worker failed; no review was published.".into())))),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// Existing setup adapters receive this retained object under their normal
/// consent gate. It supplies selected bytes, never an approval or live engine.
pub(crate) fn prepared_locked(
    app: &App,
    consent: &Consent,
    id: &str,
) -> ApiResult<Arc<PreparedCatalog>> {
    let prepared = app
        .local_catalog
        .shared
        .inner
        .lock()
        .unwrap()
        .prepared
        .clone()
        .filter(|p| p.selected.summary().id == id)
        .ok_or_else(conflict)?;
    validate_binding_locked(app, consent, &prepared.binding)?;
    Ok(prepared)
}

impl CatalogBinding {
    pub(crate) fn require_gguf_artifacts(
        &self,
        executable: &smith_managed_runtime::Artifact,
        model: &smith_managed_runtime::Artifact,
    ) -> ApiResult<()> {
        let m = &self.manifest;
        if m.description.format != catalog::ModelFormat::Gguf
            || m.runtime.files.len() != 1
            || m.model.files.len() != 1
            || m.runtime.files[0].sha256 != executable.sha256
            || m.model.files[0].sha256 != model.sha256
            || m.runtime.root.join(&m.runtime.files[0].name) != executable.path
            || m.model.root.join(&m.model.files[0].name) != model.path
        {
            return Err(bad("The GGUF setup files differ from the reviewed catalog paths or bytes, or include unsupported runtime sidecars."));
        }
        Ok(())
    }
    pub(crate) fn summary(&self) -> &catalog::ReviewSummary {
        self.review.summary()
    }
    /// A plan's own fresh hardware observation replaces its earlier snapshot.
    /// All saved selection fields and the original expiry remain unchanged.
    pub(crate) fn with_inventory_locked(
        &self,
        app: &App,
        consent: &Consent,
        generation: String,
    ) -> ApiResult<Self> {
        validate_binding_locked(app, consent, self)?;
        let saved = read(&app.db.lock().unwrap())?;
        let mut context = self.review.summary().context.clone();
        context.inventory_generation = generation;
        let review = saved
            .catalog
            .review(
                &self.review.summary().entry_id,
                self.review.summary().entry_revision,
                context,
            )
            .map_err(catalog_error)?;
        Ok(Self {
            review: Arc::new(review),
            ..self.clone()
        })
    }
}
impl PreparedCatalog {
    pub(crate) fn selected_file(
        &self,
        runtime: bool,
        role: catalog::FileRole,
    ) -> ApiResult<crate::runtime_catalog::SelectedFileInput> {
        let package = if runtime {
            &self.entry().manifest.runtime
        } else {
            &self.entry().manifest.model
        };
        let mut files = package.files.iter().filter(|f| f.role == role);
        let file = files.next().ok_or_else(conflict)?;
        if files.next().is_some() {
            return Err(conflict());
        }
        let path = package
            .root
            .join(&file.name)
            .into_os_string()
            .into_string()
            .map_err(|_| conflict())?;
        Ok(crate::runtime_catalog::SelectedFileInput {
            path,
            source: package.source.label.clone(),
            source_revision: package.source.revision.clone(),
        })
    }
    pub(crate) fn require_gguf(
        &self,
        files: &crate::runtime_catalog::CatalogSelection,
    ) -> ApiResult<()> {
        self.binding
            .require_gguf_artifacts(&files.executable, &files.model)
    }
    pub(crate) fn require_mlx(&self, plan: &smith_mlx_runtime::PlanSummary) -> ApiResult<()> {
        let m = &self.entry().manifest;
        let executable = m
            .runtime
            .files
            .iter()
            .find(|f| f.role == catalog::FileRole::Executable)
            .ok_or_else(conflict)?;
        if m.description.format != catalog::ModelFormat::Mlx
            || executable.sha256 != plan.worker.sha256
            || m.runtime.root.join(&executable.name) != plan.worker.path
            || m.model.root != plan.model.root
        {
            return Err(conflict());
        }
        let expected: std::collections::BTreeMap<_, _> = m
            .runtime
            .files
            .iter()
            .map(|f| {
                let name = if f.role == catalog::FileRole::Executable {
                    "smith-mlx-worker"
                } else {
                    f.name.as_str()
                };
                (name, (f.bytes, f.sha256.as_str()))
            })
            .collect();
        let actual: std::collections::BTreeMap<_, _> = plan
            .runtime_files
            .iter()
            .map(|f| (f.name.as_str(), (f.bytes, f.sha256.as_str())))
            .collect();
        let expected_model: std::collections::BTreeMap<_, _> = m
            .model
            .files
            .iter()
            .map(|f| (f.name.as_str(), (f.bytes, f.sha256.as_str())))
            .collect();
        let actual_model: std::collections::BTreeMap<_, _> = plan
            .model
            .files
            .iter()
            .map(|f| (f.name.as_str(), (f.bytes, f.sha256.as_str())))
            .collect();
        if expected.len() != m.runtime.files.len()
            || actual.len() != plan.runtime_files.len()
            || expected_model.len() != m.model.files.len()
            || actual_model.len() != plan.model.files.len()
            || expected != actual
            || expected_model != actual_model
        {
            return Err(bad("The MLX adapter's exact runtime resources or model package differ from the reviewed catalog files. Register a complete explicit selection before preparing."));
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(tag = "adapter", rename_all = "snake_case", deny_unknown_fields)]
enum SetupInput {
    Gguf {
        review_id: String,
        model_id: String,
        settings_revision: u64,
        expected: Expected,
    },
    Mlx {
        review_id: String,
        settings: mlx_api::CatalogSetupInput,
    },
}
async fn setup(
    State(app): State<Arc<App>>,
    Json(input): Json<SetupInput>,
) -> ApiResult<Json<Value>> {
    let review_id = match &input {
        SetupInput::Gguf { review_id, .. } | SetupInput::Mlx { review_id, .. } => review_id,
    };
    let prepared = {
        let consent = app.consent.lock().unwrap();
        prepared_locked(&app, &consent, review_id)?
    };
    match input {
        SetupInput::Gguf {
            model_id,
            settings_revision,
            expected,
            ..
        } => {
            acceleration_api::inspect_catalog(app, prepared, model_id, settings_revision, expected)
                .await
        }
        SetupInput::Mlx { settings, .. } => mlx_api::prepare_catalog(app, prepared, settings).await,
    }
}

#[cfg(test)]
#[path = "../tests/support/local_catalog_api.rs"]
mod tests;
