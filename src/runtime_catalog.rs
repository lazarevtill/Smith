//! Selected local runtime/model setup helpers. No directory discovery, downloads,
//! or automatic execution. The host retains results and separately approves a
//! RuntimeProbePlan before starting its registered, cancellable worker.
use serde::{Deserialize, Serialize};
use smith_managed_runtime::{self as native, Artifact, ArtifactKind, Cancellation, GgufMetadata};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedFileInput {
    pub path: String,
    /// A review label, never a publisher-authenticity claim or download URL.
    pub source: String,
    pub source_revision: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct CatalogSelection {
    pub executable: Artifact,
    pub model: Artifact,
    pub metadata: GgufMetadata,
    pub provenance: &'static str,
    pub inference_verified: bool,
}
#[derive(Debug)]
pub enum CatalogError {
    Runtime(native::Failure),
    Metadata(native::GgufFailure),
    Worker,
}
impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::Metadata(e) => write!(f, "GGUF: {e}"),
            Self::Worker => f.write_str("Metadata worker did not finish"),
        }
    }
}
impl std::error::Error for CatalogError {}
impl From<native::Failure> for CatalogError {
    fn from(e: native::Failure) -> Self {
        Self::Runtime(e)
    }
}

fn selected_path(input: &SelectedFileInput) -> Result<PathBuf, CatalogError> {
    if input.path.is_empty()
        || input.path.len() > 4096
        || input.path.contains('\0')
        || !Path::new(&input.path).is_absolute()
    {
        return Err(native::Failure::InvalidManifest.into());
    }
    Ok(PathBuf::from(&input.path))
}

/// Read/hash only. Never executes the selected runtime. Caller should run this
/// within its registered setup operation and keep its cancellation handle.
pub async fn inspect_selection(
    executable: SelectedFileInput,
    model: SelectedFileInput,
    cancel: &Cancellation,
) -> Result<CatalogSelection, CatalogError> {
    let executable_path = selected_path(&executable)?;
    let model_path = selected_path(&model)?;
    if cancel.is_cancelled() {
        return Err(native::Failure::Cancelled.into());
    }
    // Metadata + hash must describe the same selected file, including identity
    // and modification metadata. Hash custody is verified again at load.
    let canonical = tokio::fs::canonicalize(model_path)
        .await
        .map_err(|_| native::Failure::InvalidManifest)?;
    let before = tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| native::Failure::InvalidManifest)?;
    let metadata_path = canonical.clone();
    let metadata = tokio::task::spawn_blocking(move || native::inspect_gguf(&metadata_path))
        .await
        .map_err(|_| CatalogError::Worker)?
        .map_err(CatalogError::Metadata)?;
    let model = native::inspect_selected_artifact(
        &canonical,
        ArtifactKind::Model,
        model.source,
        model.source_revision,
        cancel,
    )
    .await?;
    let after = tokio::fs::metadata(&canonical)
        .await
        .map_err(|_| native::Failure::ArtifactChanged)?;
    if !same_file(&before, &after) {
        return Err(native::Failure::ArtifactChanged.into());
    }
    let executable = native::inspect_selected_artifact(
        &executable_path,
        ArtifactKind::Executable,
        executable.source,
        executable.source_revision,
        cancel,
    )
    .await?;
    Ok(CatalogSelection {
        executable,
        model,
        metadata,
        provenance: "explicit_local_selection_and_computed_sha256_not_publisher_verification",
        inference_verified: false,
    })
}
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev()
            || a.ino() != b.ino()
            || a.ctime() != b.ctime()
            || a.ctime_nsec() != b.ctime_nsec()
        {
            return false;
        }
    }
    a.is_file() && b.is_file() && a.len() == b.len() && a.modified().ok() == b.modified().ok()
}
