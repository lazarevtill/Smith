use serde::{Deserialize, Serialize};
use std::path::PathBuf;

macro_rules! enum_type {
    ($name:ident {$($variant:ident),+ $(,)?}) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all="snake_case")]
        pub enum $name { $($variant),+ }
    };
}
enum_type!(ModelFormat { Gguf, Mlx });
enum_type!(Origin {
    ExternalUserSelected
});
enum_type!(Completeness { SelectedFilesOnly });
enum_type!(DeclaredCapability {
    RawCompletion,
    TextChat,
    Embeddings,
    Vision,
    SpeechRecognition
});
enum_type!(FileRole {
    Executable,
    RuntimeResource,
    Weights,
    ModelMetadata
});
enum_type!(KvType { F16, Q8_0, Q4_0 });
enum_type!(FlashAttention { On, Off, Auto });

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub label: String,
    pub revision: String,
    pub origin: Origin,
}

/// These hints are passive, caller-declared values, not adapter support,
/// effective settings, qualification, a memory-fit estimate, or an Auto result.
/// Selecting an entry never applies them or changes the host's manual pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "adapter",
    content = "hints",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AdapterHints {
    LlamaGguf(GgufHints),
    MlxSwift(MlxHints),
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GgufHints {
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub threads: Option<u16>,
    pub prefill_threads: Option<u16>,
    pub batch_tokens: Option<u32>,
    pub microbatch_tokens: Option<u32>,
    pub gpu_layers: Option<u32>,
    pub kv_type: Option<KvType>,
    pub flash_attention: Option<FlashAttention>,
    pub mmap: Option<bool>,
    pub mlock: Option<bool>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MlxHints {
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    /// Integer representation avoids NaN and floating-point hash ambiguity.
    pub temperature_milli: Option<u16>,
    pub top_p_milli: Option<u16>,
    pub seed: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescription {
    pub format: ModelFormat,
    pub architecture: String,
    pub declared_capabilities: Vec<DeclaredCapability>,
    pub declared_context_limit: Option<u32>,
    pub hints: AdapterHints,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedFile {
    /// Relative spelling inside the explicitly selected package root. No scan.
    pub name: String,
    pub role: FileRole,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageSelection {
    pub root: PathBuf,
    pub source: Source,
    pub files: Vec<SelectedFile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub description: ModelDescription,
    pub runtime: PackageSelection,
    pub model: PackageSelection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub name: String,
    pub role: FileRole,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRecord {
    pub root: PathBuf,
    pub source: Source,
    pub completeness: Completeness,
    pub files: Vec<FileRecord>,
}
/// Deserializable historical data. Integrity is validated by Catalog::decode;
/// it remains unverified until an explicit prepare hashes the selected bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestRecord {
    pub description: ModelDescription,
    pub runtime: PackageRecord,
    pub model: PackageRecord,
    pub sha256: String,
}
/// Constructible only by an explicit successful full-byte capture.
/// This does not parse the model format or execute/test the adapter.
pub struct CapturedAssets(pub(crate) ManifestRecord);
impl CapturedAssets {
    pub fn manifest(&self) -> &ManifestRecord {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRecord {
    pub id: String,
    pub revision: u64,
    pub label: String,
    pub trashed: bool,
    pub manifest: ManifestRecord,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRecord {
    pub format_version: u32,
    pub revision: u64,
    pub entries: Vec<EntryRecord>,
    pub selected_for_review: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
/// Host-generated current context, not values taken on trust from a client.
/// Must be checked again by the host when background preparation completes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationContext {
    pub session_id: String,
    pub config_revision: String,
    pub inventory_generation: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    pub id: String,
    pub catalog_revision: u64,
    pub entry_id: String,
    pub entry_revision: u64,
    pub manifest_sha256: String,
    pub context: PreparationContext,
}
