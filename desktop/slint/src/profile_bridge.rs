//! Native stopped-copy import. Preflight is a real, host-owned operation that
//! rotates the profile session. It is not a frozen server plan: execution
//! re-verifies the selected copy. Reviews bind paths, directory identity and
//! the displayed profile revision, and are consumed before any execution attempt.
//! No key is retained in a workflow/review or returned by status. There is no
//! cancellation endpoint; leaving a pane discards its review, not the operation.
use crate::bridge::{Bridge, Result, Transport};
use crate::http::{wipe_json, wipe_string};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

/// Owned sensitive transport body. Implementations must not clone, log, format,
/// or retain it. Borrowing JSON for immediate serialization is deliberate.
/// This scrubs owned buffers, not every allocator or OS/network copy.
pub struct SensitiveBody(Value);
impl SensitiveBody {
    pub fn value(&self) -> &Value {
        &self.0
    }
    pub(crate) fn new(value: Value) -> Self {
        Self(value)
    }
}
impl Drop for SensitiveBody {
    fn drop(&mut self) {
        wipe_json(&mut self.0);
    }
}
/// Move-only, never Debug/Serialize. Clear masked UI fields BEFORE queueing this
/// value. Each inspection/execution needs fresh entry; no key survives an action.
pub struct OriginalKey {
    value: Value,
}
impl OriginalKey {
    pub fn master(mut master_hex: String) -> Result<Self> {
        if master_hex.len() != 64 || !master_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            wipe_string(&mut master_hex);
            return Err(
                "The original master key must contain exactly 64 hexadecimal characters.".into(),
            );
        }
        let mut value = json!({"format":"master"});
        value["master_hex"] = Value::String(master_hex);
        Ok(Self { value })
    }
    pub fn derived(mut database_passphrase: String, mut artifact_key_hex: String) -> Result<Self> {
        if database_passphrase.is_empty()
            || database_passphrase.len() > 4096
            || database_passphrase.contains('\0')
            || artifact_key_hex.len() != 64
            || !artifact_key_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            wipe_string(&mut database_passphrase);
            wipe_string(&mut artifact_key_hex);
            return Err("Enter the original database passphrase and exactly 64 hexadecimal artifact-key characters.".into());
        }
        let mut value = json!({"format":"derived"});
        value["database_passphrase"] = Value::String(database_passphrase);
        value["artifact_key_hex"] = Value::String(artifact_key_hex);
        Ok(Self { value })
    }
    fn take(&mut self) -> Value {
        std::mem::take(&mut self.value)
    }
}
impl Drop for OriginalKey {
    fn drop(&mut self) {
        wipe_json(&mut self.value);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Directory {
    path: PathBuf,
    identity: (u64, u64),
}
fn directory(path: &Path) -> Result<Directory> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err("Choose an absolute, unaliased copied directory.".into());
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| "The explicitly selected copied directory is unavailable.")?;
    if canonical != path {
        return Err(
            "Select the canonical copied directory, without symbolic links or aliases.".into(),
        );
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| "Cannot inspect the selected copied directory.")?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("Choose a copied directory, not a file or symbolic link.".into());
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    Ok(Directory {
        path: canonical,
        identity,
    })
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSelection {
    source: Directory,
    managed: Directory,
}
impl SourceSelection {
    pub fn new(
        source_copy: &Path,
        managed_copy: &Path,
        offline_copy_attested: bool,
    ) -> Result<Self> {
        if !offline_copy_attested {
            return Err(
                "Confirm these are independent copies from a stopped original application.".into(),
            );
        }
        let source = directory(source_copy)?;
        let managed = directory(managed_copy)?;
        let metadata = std::fs::symlink_metadata(source.path.join("smith.db"))
            .map_err(|_| "The selected copy must contain smith.db.")?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("The copied smith.db must be a regular file.".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() != 1 {
                return Err(
                    "The copied smith.db must not share a hard link with another profile.".into(),
                );
            }
        }
        Ok(Self { source, managed })
    }
    pub fn source_copy(&self) -> &Path {
        &self.source.path
    }
    pub fn managed_copy(&self) -> &Path {
        &self.managed.path
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Default,
    Migrated,
    Imported,
    Deactivated,
    DeactivatedImport,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfilePhase {
    Unlocked,
    Operating,
    Failed,
    Locked,
    NeedsCreation,
    MigrationRequired,
    Locking,
    LockedFault,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ImportReport {
    pub import_id: String,
    pub profile_id: Option<String>,
    pub source_schema: i64,
    pub target_schema: i64,
    pub source_tables: u64,
    pub source_rows: u64,
    pub source_files: u64,
    pub verified_blobs: u64,
    pub external_stores: u64,
    pub external_artifacts: u64,
    pub mapped_chats: u64,
    pub mapped_messages: u64,
    pub mapped_artifacts: u64,
    pub mapped_tasks: u64,
    pub projection_warnings: u64,
    pub retained_tables: u64,
    pub retained_rows: u64,
    pub retained_schema_objects: u64,
    pub mapped_domains: Vec<String>,
    pub dormant_domains: Vec<String>,
    pub verified: bool,
    pub baseline_unchanged: bool,
    pub later_writes: u64,
    pub rollback_eligible: bool,
}
#[derive(Clone, Debug, Deserialize)]
pub struct RollbackReport {
    pub eligible: bool,
    pub later_writes: u64,
    pub source_unchanged: bool,
    pub destination_unchanged: bool,
    pub migration_id: String,
    pub deactivated: bool,
}
#[derive(Clone, Debug, Deserialize)]
pub struct OperationStatus {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub error: Option<String>,
    result: Option<Value>,
}
impl OperationStatus {
    pub fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "succeeded" | "failed")
    }
    pub fn succeeded(&self) -> bool {
        self.status == "succeeded"
    }
    pub fn import_report(&self) -> Result<ImportReport> {
        if !self.succeeded() || !matches!(self.kind.as_str(), "legacy-preflight" | "legacy-import")
        {
            return Err("A verified original-copy report is unavailable.".into());
        }
        let report: ImportReport =
            serde_json::from_value(self.result.clone().unwrap_or(Value::Null))
                .map_err(|_| "Invalid original-copy report.")?;
        if !report.verified
            || report.source_schema != 25
            || ![7, 8, 9, 10, 11].contains(&report.target_schema)
        {
            return Err("The original-copy report is not verified or supported.".into());
        }
        Ok(report)
    }
    pub fn rollback_report(&self) -> Result<RollbackReport> {
        if !self.succeeded() || self.kind != "legacy-rollback" {
            return Err("A completed deactivation report is unavailable.".into());
        }
        serde_json::from_value(self.result.clone().unwrap_or(Value::Null))
            .map_err(|_| "Invalid deactivation report.".into())
    }
}
/// Revision/session deliberately have no Debug representation or public getters.
#[derive(Clone, Deserialize)]
pub struct ProfileStatus {
    pub state: ProfilePhase,
    pub selection: Selection,
    pub automatic: bool,
    pub encrypted: bool,
    revision: String,
    session: Option<String>,
    pub operation: Option<OperationStatus>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationTicket {
    pub id: String,
    pub kind: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    Import,
    Deactivate,
}
/// Non-serializable UI review, centrally consumed once even if a callback clones it.
#[derive(Clone)]
pub struct ProfileReview {
    id: String,
    action: ReviewAction,
    selection: SourceSelection,
    revision: String,
    session: String,
    report: Option<ImportReport>,
}
impl ProfileReview {
    pub fn action(&self) -> ReviewAction {
        self.action
    }
    pub fn selection(&self) -> &SourceSelection {
        &self.selection
    }
    pub fn report(&self) -> Option<&ImportReport> {
        self.report.as_ref()
    }
}
struct Pending {
    ticket: Option<OperationTicket>,
    kind: &'static str,
    selection: SourceSelection,
}
pub struct ProfileWorkflow {
    owned_root: Directory,
    pending: Option<Pending>,
    last: Option<ProfileStatus>,
    inspection: Option<(SourceSelection, ImportReport, String, String)>,
    review_id: Option<String>,
}
impl ProfileWorkflow {
    pub fn new(host_profile_root: &Path) -> Result<Self> {
        Ok(Self {
            owned_root: directory(host_profile_root)?,
            pending: None,
            last: None,
            inspection: None,
            review_id: None,
        })
    }
    pub fn invalidate_review(&mut self) {
        self.review_id = None;
        self.inspection = None;
    }
    fn validate_selection(&self, source: &SourceSelection) -> Result<()> {
        SourceSelection::new(source.source_copy(), source.managed_copy(), true)?;
        if directory(&self.owned_root.path)? != self.owned_root {
            return Err("The owned workspace directory changed. Restart the application.".into());
        }
        for selected in [&source.source, &source.managed] {
            if directory(&selected.path)? != *selected {
                return Err("A selected copy directory changed. Inspect it again.".into());
            }
            if selected.path.starts_with(&self.owned_root.path)
                || self.owned_root.path.starts_with(&selected.path)
            {
                return Err("Select independent stopped copies outside the owned live workspace and its parent directories.".into());
            }
        }
        Ok(())
    }
    fn fetch<T: Transport>(bridge: &mut Bridge<T>) -> Result<ProfileStatus> {
        let value = bridge
            .transport
            .request("GET", "/api/profile/status", None, None)?;
        let status: ProfileStatus =
            serde_json::from_value(value).map_err(|_| "Invalid automatic workspace status.")?;
        if !status.automatic || !status.encrypted {
            return Err("Copied import requires the owned automatic encrypted workspace.".into());
        }
        Ok(status)
    }
    fn available(status: &ProfileStatus) -> Result<&str> {
        if status.state != ProfilePhase::Unlocked {
            return Err("Wait for the owned workspace operation to finish.".into());
        }
        status
            .session
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "The current workspace session is unavailable.".into())
    }
    pub fn status<T: Transport>(&mut self, bridge: &mut Bridge<T>) -> Result<ProfileStatus> {
        let status = Self::fetch(bridge)?;
        if let Some(pending) = &self.pending {
            if let Some(ticket) = &pending.ticket {
                let operation=status.operation.as_ref().filter(|v|v.id==ticket.id && v.kind==ticket.kind)
                    .ok_or("The operation changed outside this review. Inspect status before continuing.")?;
                if operation.terminal() {
                    if operation.succeeded() && pending.kind == "legacy-preflight" {
                        Self::available(&status)?;
                        self.inspection = Some((
                            pending.selection.clone(),
                            operation.import_report()?,
                            status.revision.clone(),
                            Self::available(&status)?.to_owned(),
                        ));
                    }
                    self.pending = None;
                }
            }
        }
        if bridge.session.as_deref() != status.session.as_deref()
            || status.state != ProfilePhase::Unlocked
        {
            bridge.revoke();
        }
        self.last = Some(status.clone());
        Ok(status)
    }
    pub fn inspect<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        selection: SourceSelection,
        key: OriginalKey,
    ) -> Result<OperationTicket> {
        if self.pending.is_some() {
            return Err(
                "Check and open the previous operation before starting another inspection.".into(),
            );
        }
        self.validate_selection(&selection)?;
        let status = Self::fetch(bridge)?;
        Self::available(&status)?;
        if bridge.view.draft.dirty {
            return Err(
                "Save or explicitly discard the current draft before inspecting another profile."
                    .into(),
            );
        }
        self.start(
            bridge,
            "legacy-preflight",
            selection,
            status.revision,
            Some(key),
        )
    }
    pub fn review(&mut self) -> Result<ProfileReview> {
        let status = self
            .last
            .as_ref()
            .ok_or("Inspect the stopped original copies first.")?;
        let session = Self::available(status)?.to_owned();
        let (selection, report, revision, inspected_session) = self
            .inspection
            .as_ref()
            .ok_or("A successful inspection is required before review.")?;
        if &status.revision != revision || &session != inspected_session {
            return Err("The workspace changed after inspection. Inspect again.".into());
        }
        let review = ProfileReview {
            id: crate::task_bridge::new_task_request_id()?,
            action: ReviewAction::Import,
            selection: selection.clone(),
            revision: status.revision.clone(),
            session,
            report: Some(report.clone()),
        };
        self.review_id = Some(review.id.clone());
        Ok(review)
    }
    pub fn review_rollback<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        selection: SourceSelection,
    ) -> Result<ProfileReview> {
        if self.pending.is_some() {
            return Err(
                "Check and open the previous operation before reviewing deactivation.".into(),
            );
        }
        self.invalidate_review();
        self.validate_selection(&selection)?;
        let status = Self::fetch(bridge)?;
        let session = Self::available(&status)?.to_owned();
        if status.selection != Selection::Imported {
            return Err("Only the selected original import can be deactivated.".into());
        }
        if bridge.view.draft.dirty {
            return Err(
                "Save or explicitly discard the current draft before deactivating this import."
                    .into(),
            );
        }
        let review = ProfileReview {
            id: crate::task_bridge::new_task_request_id()?,
            action: ReviewAction::Deactivate,
            selection,
            revision: status.revision.clone(),
            session,
            report: None,
        };
        self.review_id = Some(review.id.clone());
        self.last = Some(status);
        Ok(review)
    }
    fn consume<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        review: &ProfileReview,
        action: ReviewAction,
    ) -> Result<()> {
        let expected = self.review_id.take();
        self.inspection = None;
        if expected.as_deref() != Some(review.id.as_str()) || review.action != action {
            return Err(
                "This review was consumed, replaced or does not match the action. Review again."
                    .into(),
            );
        }
        self.validate_selection(&review.selection)?;
        let current = Self::fetch(bridge)?;
        if Self::available(&current)? != review.session || current.revision != review.revision {
            bridge.revoke();
            return Err("The workspace changed after review. Open it and inspect again.".into());
        }
        if bridge.view.draft.dirty {
            return Err(
                "The draft changed after review. Save or explicitly discard it, then review again."
                    .into(),
            );
        }
        Ok(())
    }
    pub fn execute<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        review: ProfileReview,
        key: OriginalKey,
    ) -> Result<OperationTicket> {
        self.consume(bridge, &review, ReviewAction::Import)?;
        self.start(
            bridge,
            "legacy-import",
            review.selection,
            review.revision,
            Some(key),
        )
    }
    pub fn rollback<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        review: ProfileReview,
    ) -> Result<OperationTicket> {
        self.consume(bridge, &review, ReviewAction::Deactivate)?;
        self.start(
            bridge,
            "legacy-rollback",
            review.selection,
            review.revision,
            None,
        )
    }
    fn start<T: Transport>(
        &mut self,
        bridge: &mut Bridge<T>,
        kind: &'static str,
        selection: SourceSelection,
        revision: String,
        mut key: Option<OriginalKey>,
    ) -> Result<OperationTicket> {
        self.invalidate_review();
        self.last = None;
        let mut body = SensitiveBody::new(
            json!({"revision":revision,"source_copy":selection.source_copy(),"managed_copy":selection.managed_copy(),"offline_copy_attested":true,"confirm":true}),
        );
        if let Some(key) = &mut key {
            body.0["original_key"] = key.take();
        }
        self.pending = Some(Pending {
            ticket: None,
            kind,
            selection,
        });
        bridge.revoke(); // Before POST: old content/session can never remain usable during drain.
        let response=bridge.transport.sensitive_request("POST",&format!("/api/profile/{kind}"),None,body)
            .map_err(|_| "The request outcome is unconfirmed. Check operation status; do not automatically retry or retain the key.")?;
        let id=response["operation_id"].as_str().filter(|v|!v.is_empty()&&v.len()<=128).ok_or("The operation acknowledgement is invalid. Check workspace status without retrying.")?;
        let ticket = OperationTicket {
            id: id.to_owned(),
            kind: kind.to_owned(),
        };
        self.pending.as_mut().unwrap().ticket = Some(ticket.clone());
        Ok(ticket)
    }
    /// Explicitly adopt the current automatic workspace after polling. This does
    /// not unlock/decrypt a manual profile and never resubmits an uncertain action.
    pub fn open<T: Transport>(&mut self, bridge: &mut Bridge<T>) -> Result<()> {
        // Opening is an explicit adoption of the available workspace, not a
        // claim about an uncorrelated operation. It must remain possible after
        // a lost/malformed acknowledgement or unsupported operation report.
        let status = Self::fetch(bridge)?;
        Self::available(&status)?;
        if status.operation.as_ref().is_some_and(|op| !op.terminal()) {
            return Err("The original-copy operation is still running.".into());
        }
        if self
            .last
            .as_ref()
            .is_none_or(|last| last.revision != status.revision || last.session != status.session)
        {
            self.invalidate_review();
        }
        self.pending = None;
        self.last = Some(status.clone());
        bridge.revoke();
        if let Err(error) = bridge.initialize() {
            bridge.revoke();
            return Err(error);
        }
        if bridge.session.as_deref() != status.session.as_deref() {
            bridge.revoke();
            self.invalidate_review();
            return Err("The workspace changed while opening. Check its status again.".into());
        }
        Ok(())
    }
}
