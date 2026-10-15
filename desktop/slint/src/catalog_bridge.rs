//! Passive saved selections. Historical file metadata never becomes execution
//! authority; only the host's retained prepare/setup flow can enter inference.
use crate::bridge::{encode_component, Bridge, Config, Result, Transport};
pub use crate::http::CatalogMutationError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

// Data-only types shared with the file-custody crate. No reader, executable or
// opaque host authority is imported into the native UI.
#[path = "../../../crates/smith-local-catalog/src/types.rs"]
#[allow(dead_code)]
mod data;
pub use data::{
    AdapterHints, CaptureRequest, Completeness, DeclaredCapability, EntryRecord, FileRole,
    GgufHints, MlxHints, ModelDescription, ModelFormat, Origin, PackageSelection, SelectedFile,
    Source,
};
const MAX_REVISION: u64 = 9_007_199_254_740_991;
fn present_option<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogVersion {
    pub revision: u64,
    #[serde(deserialize_with = "present_option")]
    pub sha256: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntrySummary {
    pub id: String,
    pub revision: u64,
    pub label: String,
    pub trashed: bool,
    pub format: ModelFormat,
    pub declared_capabilities: Vec<DeclaredCapability>,
    pub origin: Origin,
    pub completeness: Completeness,
    pub selected_for_review: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Running,
    Succeeded,
    Cancelled,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSnapshot {
    pub id: String,
    pub kind: String,
    pub status: OperationPhase,
    pub request_id: Option<String>,
    pub request_sha256: Option<String>,
    pub submitted_catalog: CatalogVersion,
    pub entry_id: Option<String>,
    pub cancel_requested: bool,
    pub worker_drained: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}
impl OperationSnapshot {
    pub fn terminal(&self) -> bool {
        self.status != OperationPhase::Running && self.worker_drained
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSummary {
    pub id: String,
    pub catalog_revision: u64,
    pub entry_id: String,
    pub entry_revision: u64,
    pub manifest_sha256: String,
    pub context: PreparationContext,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationContext {
    pub session_id: String,
    pub config_revision: String,
    pub inventory_generation: String,
}
#[derive(Clone)]
pub struct CatalogPage {
    pub entries: Vec<EntrySummary>,
    pub trashed: bool,
    pub selected: Option<String>,
    pub operations: Vec<OperationSnapshot>,
    pub prepared: Option<PreparedSummary>,
    session: String,
    version: CatalogVersion,
    members: BTreeMap<String, EntrySummary>,
}
#[derive(Clone)]
pub struct CatalogEditor {
    session: String,
    version: CatalogVersion,
}
#[derive(Clone)]
pub struct EntryReview {
    editor: CatalogEditor,
    entry: EntrySummary,
}
impl EntryReview {
    pub fn entry(&self) -> &EntrySummary {
        &self.entry
    }
}
impl CatalogPage {
    pub fn version(&self) -> &CatalogVersion {
        &self.version
    }
    pub fn editor(&self) -> CatalogEditor {
        CatalogEditor {
            session: self.session.clone(),
            version: self.version.clone(),
        }
    }
    pub fn review(&self, row: &EntrySummary) -> Result<EntryReview> {
        if self.members.get(&row.id) != Some(row) {
            return Err("Choose an exact row from the displayed saved-model list.".into());
        }
        Ok(EntryReview {
            editor: self.editor(),
            entry: row.clone(),
        })
    }
    pub fn operation(&self, id: &str) -> Result<CatalogOperation> {
        let snapshot = self
            .operations
            .iter()
            .find(|o| o.id == id)
            .ok_or("Choose a retained catalog operation.")?;
        Ok(CatalogOperation {
            session: self.session.clone(),
            snapshot: snapshot.clone(),
        })
    }
}
#[derive(Clone)]
pub struct EntryDetails {
    pub entry: EntryRecord,
    review: EntryReview,
}
impl EntryDetails {
    pub fn review(&self) -> EntryReview {
        self.review.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MutationKind {
    Register,
    Replace,
    Rename,
    Trash,
    Restore,
    Select,
}
impl MutationKind {
    fn receipt_name(&self) -> &'static str {
        match self {
            Self::Register => "registered",
            Self::Replace => "replaced",
            Self::Rename => "rename_v1",
            Self::Trash => "trash_v1",
            Self::Restore => "restore_v1",
            Self::Select => "select_v1",
        }
    }
}
/// Retain this before dispatch. Explicit retries use exactly this UUID/body;
/// inspection is GET-only. It carries no permission to load or execute files.
#[derive(Clone)]
pub struct CatalogMutation {
    editor: CatalogEditor,
    request_id: String,
    method: &'static str,
    path: String,
    body: Value,
    kind: MutationKind,
    entry: Option<EntrySummary>,
    label: Option<String>,
}
impl CatalogMutation {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationReceipt {
    pub request_id: String,
    pub catalog_revision: u64,
    pub entry: EntrySummary,
    pub operation: String,
}
#[derive(Clone)]
pub struct CatalogOperation {
    session: String,
    snapshot: OperationSnapshot,
}
impl CatalogOperation {
    pub fn snapshot(&self) -> &OperationSnapshot {
        &self.snapshot
    }
}
#[derive(Clone)]
pub enum MutationOutcome {
    Committed(MutationReceipt),
    Pending(CatalogOperation),
}

#[derive(Clone)]
pub struct PrepareReview {
    entry: EntryReview,
    config: Config,
}
#[derive(Clone)]
pub struct PrepareRecovery {
    review: PrepareReview,
    before_ids: BTreeSet<String>,
}
pub enum PrepareFailure {
    NotSubmitted(String),
    OutcomeUnknown {
        message: String,
        recovery: Box<PrepareRecovery>,
    },
}
#[derive(Clone)]
pub struct PreparedReview {
    session: String,
    version: CatalogVersion,
    entry: EntrySummary,
    config: Config,
    summary: PreparedSummary,
    used: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl PreparedReview {
    pub fn summary(&self) -> &PreparedSummary {
        &self.summary
    }
    pub fn entry(&self) -> &EntrySummary {
        &self.entry
    }
    /// Human labels can contain Unicode/spaces; the captured stable ID is the
    /// adapter-safe runtime identity and does not change on a passive rename.
    pub fn runtime_model_id(&self) -> &str {
        &self.entry.id
    }
}
#[derive(Clone)]
pub enum SetupSettings {
    Gguf {
        reviewed: crate::settings::Review,
        model_id: String,
    },
    Mlx {
        reviewed: crate::mlx::MlxReview,
        model_id: String,
        capability: String,
        sampling: Value,
        thinking_mode: crate::mlx::ThinkingMode,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupAdapter {
    Gguf,
    Mlx,
}
pub enum SetupFailure {
    NotSubmitted(String),
    OutcomeUnknown(String),
}
pub struct SetupOperation {
    pub adapter: SetupAdapter,
    pub operation_id: String,
}
impl CatalogPage {
    pub fn prepare_review(&self, row: &EntrySummary, config: &Config) -> Result<PrepareReview> {
        let entry = self.review(row)?;
        if entry.entry.trashed
            || !entry.entry.selected_for_review
            || self.selected.as_deref() != Some(&row.id)
        {
            return Err(
                "Select this saved entry for review first; that selection does not load it.".into(),
            );
        }
        Ok(PrepareReview {
            entry,
            config: config.clone(),
        })
    }
    pub fn prepared_review(&self, config: &Config) -> Result<PreparedReview> {
        let summary = self
            .prepared
            .as_ref()
            .ok_or("Prepare the selected saved model first.")?;
        let entry = self
            .members
            .get(&summary.entry_id)
            .ok_or("The prepared entry is not in this displayed list.")?;
        if !entry.selected_for_review
            || self.selected.as_deref() != Some(&entry.id)
            || entry.trashed
            || summary.catalog_revision != self.version.revision
            || summary.entry_revision != entry.revision
            || summary.context.config_revision != config.revision
        {
            return Err(
                "The prepared catalog context changed; inspect the current selection again.".into(),
            );
        }
        Ok(PreparedReview {
            session: self.session.clone(),
            version: self.version.clone(),
            entry: entry.clone(),
            config: config.clone(),
            summary: summary.clone(),
            used: Default::default(),
        })
    }
}

impl CatalogEditor {
    pub fn register(&self, label: String, selection: CaptureRequest) -> Result<CatalogMutation> {
        check_label(&label)?;
        check_selection(&selection)?;
        let request_id = crate::task_bridge::new_task_request_id()?;
        Ok(CatalogMutation {
            editor: self.clone(),
            body: json!({"request_id":request_id,"catalog":self.version,"label":label,"selection":selection}),
            request_id,
            method: "POST",
            path: "/api/local-catalog/entries".into(),
            kind: MutationKind::Register,
            entry: None,
            label: Some(label),
        })
    }
}
impl EntryReview {
    fn mutation(
        &self,
        kind: MutationKind,
        label: Option<String>,
        selection: Option<CaptureRequest>,
    ) -> Result<CatalogMutation> {
        if (kind == MutationKind::Restore) != self.entry.trashed {
            return Err("This action does not match the displayed active/Trash state.".into());
        }
        if let Some(label) = &label {
            check_label(label)?;
        }
        if let Some(selection) = &selection {
            check_selection(selection)?;
        }
        let request_id = crate::task_bridge::new_task_request_id()?;
        let mut body = json!({"request_id":request_id,"catalog":self.editor.version,"entry_revision":self.entry.revision});
        if let Some(label) = &label {
            body["label"] = json!(label);
        }
        if let Some(selection) = selection {
            body["selection"] = json!(selection);
        }
        let base = format!(
            "/api/local-catalog/entries/{}",
            encode_component(&self.entry.id)
        );
        let (method, path) = match kind {
            MutationKind::Replace => ("PUT", base),
            MutationKind::Rename => ("POST", format!("{base}/rename")),
            MutationKind::Trash => ("DELETE", base),
            MutationKind::Restore => ("POST", format!("{base}/restore")),
            MutationKind::Select => {
                body["entry_id"] = json!(self.entry.id);
                ("POST", "/api/local-catalog/select".into())
            }
            MutationKind::Register => unreachable!(),
        };
        Ok(CatalogMutation {
            editor: self.editor.clone(),
            request_id,
            method,
            path,
            body,
            kind,
            entry: Some(self.entry.clone()),
            label,
        })
    }
    pub fn rename(&self, label: String) -> Result<CatalogMutation> {
        self.mutation(MutationKind::Rename, Some(label), None)
    }
    pub fn trash(&self) -> Result<CatalogMutation> {
        self.mutation(MutationKind::Trash, None, None)
    }
    pub fn restore(&self) -> Result<CatalogMutation> {
        self.mutation(MutationKind::Restore, None, None)
    }
    pub fn select(&self) -> Result<CatalogMutation> {
        self.mutation(MutationKind::Select, None, None)
    }
    pub fn replace(&self, label: String, selection: CaptureRequest) -> Result<CatalogMutation> {
        self.mutation(MutationKind::Replace, Some(label), Some(selection))
    }
}
impl<T: Transport> Bridge<T> {
    fn catalog_session(&mut self, expected: &str) -> Result<()> {
        self.check_session()?;
        if self.session.as_deref() != Some(expected) {
            return Err("This saved-model action belongs to a previous workspace.".into());
        }
        Ok(())
    }
    pub fn catalog_list(&mut self, trashed: bool) -> Result<CatalogPage> {
        self.check_session()?;
        let session = self.session.clone().ok_or("Workspace unavailable.")?;
        let v = self.get(if trashed {
            "/api/local-catalog?trashed=true"
        } else {
            "/api/local-catalog"
        })?;
        self.catalog_session(&session)?;
        if v["inspection"] != "stored_metadata_only"
            || v["automatic_load"] != false
            || v["hints_applied"] != false
        {
            return Err("Saved-model browsing returned an unexpected execution state.".into());
        }
        let version: CatalogVersion = decode(v["catalog"].clone())?;
        check_version(&version)?;
        let entries: Vec<EntrySummary> = decode(v["entries"].clone())?;
        let selected: Option<String> = decode(v["selected_for_review"].clone())?;
        if entries.len() > 128 || selected.as_ref().is_some_and(|s| !entry_id(s)) {
            return Err("Invalid saved-model list bounds or selection.".into());
        }
        let mut members = BTreeMap::new();
        for row in &entries {
            check_entry(row)?;
            if row.revision > version.revision
                || row.trashed != trashed
                || row.selected_for_review != (selected.as_deref() == Some(&row.id))
                || members.insert(row.id.clone(), row.clone()).is_some()
            {
                return Err("Saved-model list contains inconsistent identities.".into());
            }
        }
        let operations: Vec<OperationSnapshot> = decode(v["operations"].clone())?;
        if operations.len() > 32 {
            return Err("Too many retained catalog operations.".into());
        }
        let mut ids = BTreeSet::new();
        for op in &operations {
            check_operation(op)?;
            if !ids.insert(op.id.clone()) {
                return Err("Repeated catalog operation identity.".into());
            }
        }
        let prepared: Option<PreparedSummary> = decode(v["prepared"].clone())?;
        if let Some(p) = &prepared {
            check_prepared(p, &session)?;
        }
        Ok(CatalogPage {
            entries,
            trashed,
            selected,
            operations,
            prepared,
            session,
            version,
            members,
        })
    }
    pub fn catalog_details(&mut self, review: &EntryReview) -> Result<EntryDetails> {
        self.catalog_session(&review.editor.session)?;
        let v = self.get(&format!(
            "/api/local-catalog/entries/{}",
            encode_component(&review.entry.id)
        ))?;
        self.catalog_session(&review.editor.session)?;
        if v["inspection"] != "stored_metadata_only" || v["file_availability"] != "not_checked" {
            return Err("Catalog details unexpectedly inspected files.".into());
        }
        let version: CatalogVersion = decode(v["catalog"].clone())?;
        let entry: EntryRecord = decode(v["entry"].clone())?;
        if version != review.editor.version
            || entry.id != review.entry.id
            || entry.revision != review.entry.revision
            || entry.label != review.entry.label
            || entry.trashed != review.entry.trashed
            || entry.manifest.description.format != review.entry.format
            || entry.manifest.description.declared_capabilities
                != review.entry.declared_capabilities
            || !sha(&entry.manifest.sha256)
        {
            return Err(
                "Catalog details changed since the displayed selection. Refresh the list.".into(),
            );
        }
        for package in [&entry.manifest.runtime, &entry.manifest.model] {
            if !historical_root(&package.root)
                || package.files.len() > 128
                || package
                    .files
                    .iter()
                    .any(|f| !sha(&f.sha256) || f.bytes > 64 * 1024 * 1024 * 1024)
            {
                return Err("Saved file metadata exceeds the catalog bounds.".into());
            }
        }
        Ok(EntryDetails {
            entry,
            review: review.clone(),
        })
    }
    pub fn catalog_submit(&mut self, mutation: &CatalogMutation) -> Result<MutationOutcome> {
        self.catalog_submit_checked(mutation).map_err(|e| match e {
            CatalogMutationError::NotSubmitted(e) | CatalogMutationError::OutcomeUnknown(e) => e,
        })
    }
    pub fn catalog_submit_checked(
        &mut self,
        mutation: &CatalogMutation,
    ) -> std::result::Result<MutationOutcome, CatalogMutationError> {
        self.catalog_session(&mutation.editor.session)
            .map_err(CatalogMutationError::NotSubmitted)?;
        let value = self.transport.catalog_mutation(
            mutation.method,
            &mutation.path,
            &mutation.editor.session,
            mutation.body.clone(),
        )?;
        self.catalog_session(&mutation.editor.session)
            .map_err(CatalogMutationError::OutcomeUnknown)?;
        mutation_outcome(mutation, value).map_err(CatalogMutationError::OutcomeUnknown)
    }
    /// GET-only inspection after an unknown result. An absent receipt is never
    /// treated as success; the retained mutation may be explicitly retried.
    pub fn catalog_recover(&mut self, mutation: &CatalogMutation) -> Result<MutationOutcome> {
        self.catalog_session(&mutation.editor.session)?;
        let v = self.get(&format!(
            "/api/local-catalog/requests/{}",
            mutation.request_id
        ))?;
        self.catalog_session(&mutation.editor.session)?;
        if v["missing_receipt_is_not_success"] != true {
            return Err("Invalid catalog recovery response.".into());
        }
        if !v["receipt"].is_null() {
            if v["receipt"]["request_id"] != mutation.request_id
                || !v["receipt"]["request_sha256"].as_str().is_some_and(sha)
            {
                return Err("Catalog receipt belongs to another request.".into());
            }
            return checked_receipt(mutation, &v["receipt"]["result"], &v["catalog"])
                .map(MutationOutcome::Committed);
        }
        checked_mutation_operation(mutation, &v["operation"]).map(MutationOutcome::Pending)
    }
    pub fn catalog_poll(&mut self, operation: &CatalogOperation) -> Result<CatalogOperation> {
        self.catalog_session(&operation.session)?;
        let v = self.get(&format!(
            "/api/local-catalog/operations/{}",
            operation.snapshot.id
        ))?;
        self.catalog_session(&operation.session)?;
        operation_response(operation, v)
    }
    pub fn catalog_cancel(&mut self, operation: &CatalogOperation) -> Result<CatalogOperation> {
        self.catalog_session(&operation.session)?;
        if operation.snapshot.status != OperationPhase::Running {
            return Err("This catalog operation is already terminal.".into());
        }
        let v = self.transport.request(
            "POST",
            &format!(
                "/api/local-catalog/operations/{}/cancel",
                operation.snapshot.id
            ),
            Some(&operation.session),
            Some(json!({})),
        )?;
        self.catalog_session(&operation.session)?;
        operation_response(operation, v)
    }
    pub fn catalog_mutation_completion(
        &mut self,
        mutation: &CatalogMutation,
        operation: &CatalogOperation,
    ) -> Result<Option<MutationReceipt>> {
        self.catalog_session(&mutation.editor.session)?;
        checked_mutation_operation(
            mutation,
            &serde_json::to_value(&operation.snapshot).map_err(|_| "Invalid catalog operation.")?,
        )?;
        if operation.session != mutation.editor.session {
            return Err("Operation belongs to another workspace.".into());
        }
        if operation.snapshot.status == OperationPhase::Succeeded {
            let value = operation
                .snapshot
                .result
                .as_ref()
                .ok_or("Catalog completion lacks its receipt.")?;
            match mutation_outcome(mutation, value.clone())? {
                MutationOutcome::Committed(r) => Ok(Some(r)),
                _ => Err("Catalog completion cannot still be pending.".into()),
            }
        } else {
            Ok(None)
        }
    }
    /// Observe the current retained-operation watermark immediately before the
    /// POST. Known preflight refusals are distinct from a lost admission reply.
    pub fn catalog_prepare(
        &mut self,
        review: &PrepareReview,
    ) -> std::result::Result<CatalogOperation, PrepareFailure> {
        let before = (|| -> Result<CatalogPage> {
            self.catalog_session(&review.entry.editor.session)?;
            if self.view.config != review.config {
                return Err("The displayed connection changed before catalog preparation.".into());
            }
            let page = self.catalog_list(false)?;
            if page.version != review.entry.editor.version
                || page.selected.as_deref() != Some(&review.entry.entry.id)
                || page.members.get(&review.entry.entry.id) != Some(&review.entry.entry)
            {
                return Err("The saved selection changed before preparation.".into());
            }
            Ok(page)
        })()
        .map_err(PrepareFailure::NotSubmitted)?;
        let recovery = PrepareRecovery {
            review: review.clone(),
            before_ids: before.operations.iter().map(|o| o.id.clone()).collect(),
        };
        let admitted = self.transport.catalog_mutation("POST", "/api/local-catalog/prepare", &review.entry.editor.session, json!({"catalog":review.entry.editor.version,"entry_id":review.entry.entry.id,"entry_revision":review.entry.entry.revision,"expected":expected(&review.config)}));
        let v = match admitted {
            Ok(v) => v,
            Err(CatalogMutationError::NotSubmitted(message)) => {
                return Err(PrepareFailure::NotSubmitted(message))
            }
            Err(CatalogMutationError::OutcomeUnknown(message)) => {
                return Err(PrepareFailure::OutcomeUnknown {
                    message,
                    recovery: Box::new(recovery),
                })
            }
        };
        let result = (|| -> Result<CatalogOperation> {
            self.catalog_session(&review.entry.editor.session)?;
            let op = prepare_operation(review, &v["operation"])?;
            if v["operation_id"] != op.snapshot.id {
                return Err("Catalog preparation returned a different operation identity.".into());
            }
            Ok(op)
        })();
        result.map_err(|message| PrepareFailure::OutcomeUnknown {
            message,
            recovery: Box::new(recovery),
        })
    }
    pub fn catalog_recover_prepare(
        &mut self,
        recovery: &PrepareRecovery,
    ) -> Result<CatalogOperation> {
        self.catalog_session(&recovery.review.entry.editor.session)?;
        let page = self.catalog_list(false)?;
        let candidates: Vec<_> = page
            .operations
            .iter()
            .filter(|o| !recovery.before_ids.contains(&o.id))
            .filter_map(|o| {
                serde_json::to_value(o)
                    .ok()
                    .and_then(|v| prepare_operation(&recovery.review, &v).ok())
            })
            .collect();
        if candidates.len() != 1 {
            return Err("No unique new preparation receipt matches this action. Do not replay it automatically; refresh the catalog status.".into());
        }
        Ok(candidates.into_iter().next().unwrap())
    }
    /// Start the existing hash/probe-review setup flow. This method never calls
    /// apply/load, changes manual preferences, or forwards saved tuning hints.
    pub fn catalog_setup(
        &mut self,
        review: &PreparedReview,
        settings: SetupSettings,
    ) -> Result<SetupOperation> {
        self.catalog_setup_checked(review, settings)
            .map_err(|e| match e {
                SetupFailure::NotSubmitted(e) | SetupFailure::OutcomeUnknown(e) => e,
            })
    }
    pub fn catalog_setup_checked(
        &mut self,
        review: &PreparedReview,
        settings: SetupSettings,
    ) -> std::result::Result<SetupOperation, SetupFailure> {
        let (adapter,body)=(||->Result<(SetupAdapter,Value)>{
        self.catalog_session(&review.session)?;
        if self.view.config != review.config {
            return Err("Connection changed; prepare the saved model again.".into());
        }
        let (adapter, body) = match settings {
            SetupSettings::Gguf { reviewed, model_id } => {
                check_runtime_model_id(&model_id)?;
                if review.entry.format != ModelFormat::Gguf
                    || reviewed.config != review.config
                    || self.view.settings.setup["preferences"]["revision"].as_u64()
                        != Some(reviewed.settings_revision)
                {
                    return Err("GGUF setup must use the displayed current settings and reviewed GGUF entry.".into());
                }
                (
                    SetupAdapter::Gguf,
                    json!({"adapter":"gguf","review_id":review.summary.id,"model_id":model_id,"settings_revision":reviewed.settings_revision,"expected":expected(&review.config)}),
                )
            }
            SetupSettings::Mlx {
                reviewed,
                model_id,
                capability,
                sampling,
                thinking_mode,
            } => {
                check_runtime_model_id(&model_id)?;
                if review.entry.format != ModelFormat::Mlx
                    || reviewed.config != review.config
                    || self.view.mlx["settings"]["revision"].as_u64()
                        != Some(reviewed.settings_revision)
                    || !matches!(capability.as_str(), "chat" | "raw_completion")
                {
                    return Err(
                        "MLX setup must use the displayed current settings and reviewed MLX entry."
                            .into(),
                    );
                }
                let mut body = json!({"adapter":"mlx","review_id":review.summary.id,"settings":{"model_id":model_id,"capability":capability,"sampling":sampling,"settings_revision":reviewed.settings_revision,"expected":expected(&review.config)}});
                if crate::mlx::supports_thinking_control(&self.view.mlx) {
                    body["settings"]["thinking_mode"] = json!(thinking_mode);
                } else if thinking_mode != crate::mlx::ThinkingMode::ModelDefault {
                    return Err("The backend does not expose reviewed MLX thinking control.".into());
                }
                (SetupAdapter::Mlx, body)
            }
        };
        // Check the retained fingerprint as well as the host's final binding.
        // A fresh observation never upgrades the already captured review.
        let current = self.catalog_list(false)?;
        if current.version != review.version
            || current.prepared.as_ref() != Some(&review.summary)
            || current.members.get(&review.entry.id) != Some(&review.entry)
        {
            return Err("Saved-model preparation changed. Review the current entry again.".into());
        }
        if review.used.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Err("This setup handoff was already attempted. Inspect Inference status before preparing another handoff.".into());
        }
            Ok((adapter,body))
        })().map_err(SetupFailure::NotSubmitted)?;
        let v = self
            .transport
            .catalog_mutation("POST", "/api/local-catalog/setup", &review.session, body)
            .map_err(|e| match e {
                CatalogMutationError::NotSubmitted(e) => {
                    // The host confirmed this first attempt admitted no work.
                    // A later explicit click still revalidates the same review.
                    review
                        .used
                        .store(false, std::sync::atomic::Ordering::Release);
                    SetupFailure::NotSubmitted(e)
                }
                CatalogMutationError::OutcomeUnknown(e) => SetupFailure::OutcomeUnknown(e),
            })?;
        (||->Result<SetupOperation>{
        self.catalog_session(&review.session)?;
        let operation_id=v["operation_id"].as_str().filter(|s|uuid(s)).ok_or("Setup handoff outcome is unconfirmed. Inspect Inference status; do not repeat the request.")?.to_owned();
        Ok(SetupOperation {
            adapter,
            operation_id,
        })
        })().map_err(SetupFailure::OutcomeUnknown)
    }
}
fn prepare_operation(review: &PrepareReview, v: &Value) -> Result<CatalogOperation> {
    let snapshot: OperationSnapshot = decode(v.clone())?;
    check_operation(&snapshot)?;
    if snapshot.kind != "prepare"
        || snapshot.request_id.is_some()
        || snapshot.request_sha256.is_some()
        || snapshot.submitted_catalog != review.entry.editor.version
        || snapshot.entry_id.as_deref() != Some(&review.entry.entry.id)
    {
        return Err("Catalog preparation belongs to another entry or version.".into());
    }
    if snapshot.status == OperationPhase::Succeeded {
        let result = snapshot
            .result
            .as_ref()
            .ok_or("Prepared catalog result is missing.")?;
        let prepared: PreparedSummary = decode(result["prepared"].clone())?;
        check_prepared(&prepared, &review.entry.editor.session)?;
        if result["entry"] != review.entry.entry.id
            || result["execution"] != "none"
            || result["qualification"] != "not_established"
            || result["hints_applied"] != false
            || prepared.entry_id != review.entry.entry.id
            || prepared.entry_revision != review.entry.entry.revision
            || prepared.catalog_revision != review.entry.editor.version.revision
            || prepared.context.config_revision != review.config.revision
        {
            return Err("Prepared selection changed its reviewed context.".into());
        }
    }
    Ok(CatalogOperation {
        session: review.entry.editor.session.clone(),
        snapshot,
    })
}
fn check_runtime_model_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err("The runtime model ID must use at most 160 ASCII letters, numbers, dot, underscore or hyphen.".into());
    }
    Ok(())
}
fn expected(c: &Config) -> Value {
    json!({"revision":c.revision,"provider":c.provider,"endpoint":c.endpoint})
}
fn mutation_outcome(m: &CatalogMutation, v: Value) -> Result<MutationOutcome> {
    if !v["receipt"].is_null() {
        if v["execution"] != "none" || v["model_files_changed"] != false {
            return Err("Catalog mutation returned an unexpected effect.".into());
        }
        checked_receipt(m, &v["receipt"], &v["catalog"]).map(MutationOutcome::Committed)
    } else {
        let op = checked_mutation_operation(m, &v["operation"])?;
        if v["operation_id"] != op.snapshot.id {
            return Err("Catalog admission identity is inconsistent.".into());
        }
        Ok(MutationOutcome::Pending(op))
    }
}
fn checked_receipt(m: &CatalogMutation, value: &Value, version: &Value) -> Result<MutationReceipt> {
    let r: MutationReceipt = decode(value.clone())?;
    check_entry(&r.entry)?;
    let version: CatalogVersion = decode(version.clone())?;
    check_version(&version)?;
    let next = m
        .editor
        .version
        .revision
        .checked_add(1)
        .ok_or("Catalog revision limit reached.")?;
    if r.request_id != m.request_id
        || r.catalog_revision != next
        || version.revision < next
        || r.operation != m.kind.receipt_name()
    {
        return Err("Catalog mutation receipt does not match the retained request.".into());
    }
    if let Some(old) = &m.entry {
        if r.entry.id != old.id
            || r.entry.revision
                != if m.kind == MutationKind::Select {
                    old.revision
                } else {
                    next
                }
        {
            return Err("Catalog entry receipt changed identity or revision.".into());
        }
        if m.kind != MutationKind::Replace
            && (r.entry.format != old.format
                || r.entry.declared_capabilities != old.declared_capabilities)
        {
            return Err("Metadata-only action changed model selection.".into());
        }
    } else if r.entry.id != format!("local-{next:016x}") || r.entry.revision != next {
        return Err("Registered catalog entry has an unexpected identity.".into());
    }
    let expected_selected = match m.kind {
        MutationKind::Select => true,
        MutationKind::Register | MutationKind::Trash | MutationKind::Restore => false,
        _ => m.entry.as_ref().is_some_and(|e| e.selected_for_review),
    };
    if r.entry.selected_for_review != expected_selected
        || m.label.is_none()
            && m.entry
                .as_ref()
                .is_some_and(|old| old.label != r.entry.label)
        || matches!(m.kind, MutationKind::Register | MutationKind::Replace)
            && (serde_json::to_value(r.entry.format).ok().as_ref()
                != Some(&m.body["selection"]["description"]["format"])
                || serde_json::to_value(&r.entry.declared_capabilities)
                    .ok()
                    .as_ref()
                    != Some(&m.body["selection"]["description"]["declared_capabilities"]))
    {
        return Err(
            "Catalog receipt changed the saved selection beyond the reviewed action.".into(),
        );
    }
    if m.label
        .as_ref()
        .is_some_and(|label| r.entry.label != *label)
        || r.entry.trashed != (m.kind == MutationKind::Trash)
        || m.kind == MutationKind::Select && !r.entry.selected_for_review
        || m.kind == MutationKind::Restore && r.entry.selected_for_review
    {
        return Err("Catalog action receipt does not match its requested effect.".into());
    }
    Ok(r)
}
fn checked_mutation_operation(m: &CatalogMutation, v: &Value) -> Result<CatalogOperation> {
    let op: OperationSnapshot = decode(v.clone())?;
    check_operation(&op)?;
    let kind = match m.kind {
        MutationKind::Register => "register",
        MutationKind::Replace => "replace",
        _ => return Err("This metadata mutation cannot run as a file worker.".into()),
    };
    if op.kind != kind
        || op.request_id.as_deref() != Some(&m.request_id)
        || op.submitted_catalog != m.editor.version
        || op.entry_id.as_deref() != m.entry.as_ref().map(|e| e.id.as_str())
    {
        return Err("Catalog worker belongs to another request.".into());
    }
    Ok(CatalogOperation {
        session: m.editor.session.clone(),
        snapshot: op,
    })
}
fn operation_response(old: &CatalogOperation, v: Value) -> Result<CatalogOperation> {
    let next: OperationSnapshot = decode(v["operation"].clone())?;
    check_operation(&next)?;
    if next.id != old.snapshot.id
        || next.kind != old.snapshot.kind
        || next.request_id != old.snapshot.request_id
        || next.request_sha256 != old.snapshot.request_sha256
        || next.submitted_catalog != old.snapshot.submitted_catalog
        || next.entry_id != old.snapshot.entry_id
        || old.snapshot.terminal() && next != old.snapshot
    {
        return Err("Catalog operation changed its captured identity or terminal result.".into());
    }
    Ok(CatalogOperation {
        session: old.session.clone(),
        snapshot: next,
    })
}
fn decode<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|_| "Invalid saved-model response.".into())
}
fn sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn entry_id(s: &str) -> bool {
    s.strip_prefix("local-").is_some_and(|v| {
        v.len() == 16
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
fn check_version(v: &CatalogVersion) -> Result<()> {
    if v.revision > MAX_REVISION
        || v.sha256.as_ref().is_some_and(|s| !sha(s))
        || v.sha256.is_none() && v.revision != 0
    {
        Err("Invalid catalog revision/fingerprint.".into())
    } else {
        Ok(())
    }
}
fn check_label(s: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control) {
        Err("Use a model name of 1–256 bytes without control characters.".into())
    } else {
        Ok(())
    }
}
fn historical_root(path: &std::path::Path) -> bool {
    let Some(s) = path.to_str() else { return false };
    !s.is_empty()
        && s.len() <= 4096
        && !s.chars().any(char::is_control)
        && (s.starts_with('/')
            || (s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && s.as_bytes().get(1) == Some(&b':')
                && s.as_bytes().get(2) == Some(&b'\\')))
}
fn check_entry(e: &EntrySummary) -> Result<()> {
    check_label(&e.label)?;
    if !entry_id(&e.id)
        || !e
            .id
            .strip_prefix("local-")
            .and_then(|id| u64::from_str_radix(id, 16).ok())
            .is_some_and(|birth| birth > 0 && birth <= e.revision)
        || e.revision == 0
        || e.revision > MAX_REVISION
        || e.declared_capabilities.len() > 5
        || e.trashed && e.selected_for_review
    {
        Err("Invalid saved-model entry.".into())
    } else {
        Ok(())
    }
}
fn check_operation(o: &OperationSnapshot) -> Result<()> {
    check_version(&o.submitted_catalog)?;
    if !uuid(&o.id)
        || !matches!(o.kind.as_str(), "register" | "replace" | "prepare")
        || o.request_id.as_ref().is_some_and(|s| !uuid(s))
        || o.request_sha256.as_ref().is_some_and(|s| !sha(s))
        || o.entry_id.as_ref().is_some_and(|s| !entry_id(s))
        || o.error.as_ref().is_some_and(|e| e.len() > 4096)
        || o.status == OperationPhase::Running && (o.worker_drained || o.result.is_some())
        || o.status == OperationPhase::Succeeded && (!o.worker_drained || o.result.is_none())
    {
        return Err("Invalid catalog operation status.".into());
    }
    Ok(())
}
fn check_prepared(p: &PreparedSummary, session: &str) -> Result<()> {
    if !sha(&p.id)
        || !sha(&p.manifest_sha256)
        || !entry_id(&p.entry_id)
        || p.entry_revision == 0
        || p.entry_revision > p.catalog_revision
        || p.catalog_revision > MAX_REVISION
        || p.context.session_id != session
        || p.context.config_revision.is_empty()
        || p.context.inventory_generation.is_empty()
    {
        Err("Invalid prepared catalog selection.".into())
    } else {
        Ok(())
    }
}
fn check_selection(s: &CaptureRequest) -> Result<()> {
    if s.description.architecture.is_empty()
        || s.description.architecture.len() > 128
        || s.description.architecture.chars().any(char::is_control)
        || s.description.declared_capabilities.len() > 5
    {
        return Err("Bound the declared model description.".into());
    }
    for package in [&s.runtime, &s.model] {
        if !package.root.is_absolute()
            || package.root.to_string_lossy().len() > 4096
            || package.files.is_empty()
            || package.files.len() > 128
        {
            return Err("Select explicit local package roots and 1–128 named files.".into());
        }
        let mut names = BTreeSet::new();
        for file in &package.files {
            let path = std::path::Path::new(&file.name);
            if file.name.is_empty()
                || file.name.chars().any(char::is_control)
                || file.name.len() > 512
                || file.name.contains(['\\', ':'])
                || file.name.split('/').count() > 16
                || file
                    .name
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
                || path.is_absolute()
                || !path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                || !names.insert(&file.name)
            {
                return Err("Use unique relative file names inside the chosen package.".into());
            }
        }
    }
    Ok(())
}

/// Convert only explicitly chosen paths into named capture inputs. This does
/// not touch the filesystem; the host performs bounded hashing on Save.
pub fn gguf_selection(
    executable: &std::path::Path,
    model: &std::path::Path,
) -> Result<CaptureRequest> {
    let source = Source {
        label: "User-selected local file".into(),
        revision: "explicit-selection".into(),
        origin: Origin::ExternalUserSelected,
    };
    let package = |path: &std::path::Path, role| -> Result<PackageSelection> {
        if !path.is_absolute() {
            return Err("Choose an absolute local file path.".into());
        }
        Ok(PackageSelection {
            root: path
                .parent()
                .ok_or("Choose a file inside a directory.")?
                .into(),
            source: source.clone(),
            files: vec![SelectedFile {
                name: path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .ok_or("Choose a UTF-8 file name.")?
                    .into(),
                role,
            }],
        })
    };
    let selection = CaptureRequest {
        description: ModelDescription {
            format: ModelFormat::Gguf,
            architecture: "unspecified".into(),
            declared_capabilities: vec![DeclaredCapability::TextChat],
            declared_context_limit: None,
            hints: AdapterHints::LlamaGguf(GgufHints::default()),
        },
        runtime: package(executable, FileRole::Executable)?,
        model: package(model, FileRole::Weights)?,
    };
    check_selection(&selection)?;
    Ok(selection)
}
/// Use the exact named files already displayed in an explicit MLX preparation.
/// The host rehashes them; no directory enumeration or execution occurs here.
pub fn mlx_selection(review: &crate::mlx::MlxReview) -> Result<CaptureRequest> {
    let plan = &review.prepared["plan"];
    let worker = std::path::Path::new(
        plan["worker"]["path"]
            .as_str()
            .ok_or("Inspect the selected MLX package in Inference first.")?,
    );
    let mut selection = gguf_selection(worker, worker)?;
    selection.description.format = ModelFormat::Mlx;
    selection.description.hints = AdapterHints::MlxSwift(MlxHints::default());
    selection.description.declared_capabilities =
        vec![if review.prepared["capability"] == "raw_completion" {
            DeclaredCapability::RawCompletion
        } else {
            DeclaredCapability::TextChat
        }];
    selection.runtime.source = Source {
        label: plan["worker"]["source"]
            .as_str()
            .ok_or("MLX worker provenance is missing.")?
            .into(),
        revision: plan["worker"]["source_revision"]
            .as_str()
            .ok_or("MLX worker revision is missing.")?
            .into(),
        origin: Origin::ExternalUserSelected,
    };
    let files = plan["runtime_files"]
        .as_array()
        .ok_or("The reviewed MLX resource list is missing.")?;
    if files.len() != 6 {
        return Err("Save the full reviewed native MLX runtime with its six resources.".into());
    }
    for f in files {
        selection.runtime.files.push(SelectedFile {
            name: f["name"]
                .as_str()
                .ok_or("Invalid MLX resource name.")?
                .into(),
            role: FileRole::RuntimeResource,
        });
    }
    selection.model.root = plan["model"]["root"]
        .as_str()
        .ok_or("MLX model directory is missing.")?
        .into();
    selection.model.files = plan["model"]["files"]
        .as_array()
        .ok_or("The reviewed model file list is missing.")?
        .iter()
        .map(|f| {
            let name = f["name"].as_str().ok_or("Invalid MLX model file name.")?;
            Ok(SelectedFile {
                name: name.into(),
                role: if name.ends_with(".safetensors") {
                    FileRole::Weights
                } else {
                    FileRole::ModelMetadata
                },
            })
        })
        .collect::<Result<_>>()?;
    check_selection(&selection)?;
    Ok(selection)
}
