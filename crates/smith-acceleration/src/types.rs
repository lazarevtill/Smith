use serde::{Deserialize, Serialize};

macro_rules! enum_type {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
enum_type!(OperatingSystem {
    Macos,
    Windows,
    Linux
});
enum_type!(Architecture { Aarch64, X86_64 });
enum_type!(DeviceClass { Cpu, Gpu, Npu });
enum_type!(Vendor {
    Apple,
    Nvidia,
    Amd,
    Intel,
    Qualcomm,
    Other,
    Unknown
});
enum_type!(Backend {
    LlamaCpu,
    LlamaMetal,
    MlxMetal,
    LlamaCuda,
    LlamaHip,
    LlamaSycl,
    LlamaVulkan
});
enum_type!(Capability {
    TextChat,
    Embeddings,
    Vision,
    ImageGeneration,
    SpeechRecognition,
    SpeechSynthesis
});
enum_type!(ModelFormat {
    GgufBundle,
    MlxBundle
});
enum_type!(Availability {
    NotInstalled,
    InstalledUnverified,
    Qualified
});
enum_type!(Mode { Auto, Manual });
enum_type!(Preference {
    Balanced,
    Fast,
    Quiet
});
enum_type!(LocalFallback {
    Never,
    Ask,
    AllowVerifiedLocal
});
enum_type!(KvType {
    F32,
    F16,
    Bf16,
    Q8,
    Q4
});
enum_type!(MemoryLocation { Host, Device });

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareInventory {
    pub generation: String,
    pub os: OperatingSystem,
    pub architecture: Architecture,
    pub os_version: String,
    pub logical_cpu_count: u16,
    pub host_memory_pool_id: String,
    pub memory_pools: Vec<MemoryPool>,
    pub devices: Vec<Device>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPool {
    pub id: String,
    pub total_bytes: Option<u64>,
    /// Current available bytes; a recommended working-set limit is not this.
    pub available_bytes: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    pub class: DeviceClass,
    pub vendor: Vendor,
    pub memory_pool_id: String,
    pub driver_version: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifiedDevice {
    pub device_id: String,
    pub os_version: String,
    pub driver_version: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterDescriptor {
    pub id: String,
    pub backend: Backend,
    pub os: OperatingSystem,
    pub architecture: Architecture,
    pub version: String,
    /// Complete package/dependency identity, supplied by trusted host validation.
    pub bundle_sha256: Option<String>,
    /// Main executable only. This does not identify dependencies/model files
    /// and can support an unvalidated settings preview, never qualification.
    pub selected_executable_sha256: Option<String>,
    pub availability: Availability,
    pub capabilities: Vec<Capability>,
    pub qualified_devices: Vec<QualifiedDevice>,
    pub settings: SettingsLimits,
    pub models: Vec<ModelProfile>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub id: String,
    pub bundle_sha256: String,
    pub format: ModelFormat,
    pub architecture: String,
    pub quantization: String,
    pub capabilities: Vec<Capability>,
    pub native_context_limit: u32,
}
/// Memory coefficients are engine/model-specific upper bounds, not inferred
/// from file size. A profile binds the exact model, KV type and offload setting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub model_bundle_sha256: String,
    pub format: ModelFormat,
    pub architecture: String,
    pub quantization: String,
    pub kv_type: KvType,
    /// None means this adapter has no layer-offload control (e.g. MLX).
    pub gpu_layers: Option<u32>,
    pub memory: Option<MemoryModel>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryModel {
    pub provenance: String,
    pub allocations: Vec<MemoryAllocation>,
}
/// One allocation is counted once in each distinct physical pool it occupies.
/// Host+Device locations alias on unified RAM, but require two copies when the
/// host and GPU pools differ. Separate scratch/staging buffers use distinct IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAllocation {
    pub id: String,
    pub locations: Vec<MemoryLocation>,
    pub base_bytes: u64,
    pub bytes_per_context_token: u64,
    pub bytes_per_batch_token: u64,
    pub bytes_per_thread: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsLimits {
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub max_threads: u16,
    pub max_batch_tokens: u32,
    pub max_microbatch_tokens: u32,
    pub supports_flash_attention: bool,
    pub supports_mmap: bool,
    pub supports_mlock: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    pub backend: Option<Backend>,
    pub device_id: Option<String>,
    pub context_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub threads: Option<u16>,
    pub prefill_threads: Option<u16>,
    pub batch_tokens: Option<u32>,
    pub microbatch_tokens: Option<u32>,
    pub gpu_layers: Option<u32>,
    pub kv_type: Option<KvType>,
    pub flash_attention: Option<bool>,
    pub mmap: Option<bool>,
    pub mlock: Option<bool>,
    pub host_budget_bytes: Option<u64>,
    pub device_budget_bytes: Option<u64>,
    pub headroom_bytes: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationIntent {
    pub mode: Mode,
    pub preference: Preference,
    pub capability: Capability,
    pub local_fallback: LocalFallback,
    pub overrides: Overrides,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSettings {
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub threads: u16,
    pub prefill_threads: u16,
    pub batch_tokens: u32,
    pub microbatch_tokens: u32,
    pub gpu_layers: Option<u32>,
    pub kv_type: KvType,
    pub flash_attention: bool,
    pub mmap: bool,
    pub mlock: bool,
    pub headroom_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolEstimate {
    pub pool_id: String,
    pub required_bytes: u64,
    pub headroom_bytes: u64,
    pub available_bytes: u64,
    pub permitted_bytes: u64,
}
enum_type!(ReasonCode {
    NoAdapters,
    Unavailable,
    PlatformMismatch,
    DeviceMismatch,
    QualificationChanged,
    ModelUnsupported,
    CapabilityUnsupported,
    OverrideUnsatisfied,
    SettingsUnsupported,
    MemoryUnknown,
    MemoryInsufficient,
    ArithmeticOverflow,
    DeterministicUnmeasuredChoice,
    AdditionalCandidatesRejected
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reason {
    pub code: ReasonCode,
    pub adapter_id: Option<String>,
    pub device_id: Option<String>,
    pub message: String,
}
/// Read-only output, not deserializable execution authority. Persistence should
/// retain intent/inputs and recompute; revalidate before any runtime admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EffectivePlan {
    pub(crate) id: String,
    pub(crate) inventory_generation: String,
    pub(crate) inventory_sha256: String,
    pub(crate) adapter_sha256: String,
    pub(crate) model_sha256: String,
    pub(crate) intent_sha256: String,
    pub(crate) config_revision: u64,
    pub(crate) adapter_id: String,
    pub(crate) adapter_bundle_sha256: String,
    pub(crate) model_bundle_sha256: String,
    pub(crate) backend: Backend,
    pub(crate) device_id: String,
    pub(crate) settings: EffectiveSettings,
    pub(crate) memory: Vec<PoolEstimate>,
    pub(crate) profile_sha256: String,
}
impl EffectivePlan {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn inventory_generation(&self) -> &str {
        &self.inventory_generation
    }
    pub fn config_revision(&self) -> u64 {
        self.config_revision
    }
    pub fn intent_sha256(&self) -> &str {
        &self.intent_sha256
    }
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }
    pub fn adapter_bundle_sha256(&self) -> &str {
        &self.adapter_bundle_sha256
    }
    pub fn model_bundle_sha256(&self) -> &str {
        &self.model_bundle_sha256
    }
    pub fn backend(&self) -> Backend {
        self.backend
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn settings(&self) -> &EffectiveSettings {
        &self.settings
    }
    pub fn memory(&self) -> &[PoolEstimate] {
        &self.memory
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
// One bounded result is returned per planning call; keep the public owned plan
// value direct instead of adding allocation/indirection to the integration API.
#[allow(clippy::large_enum_variant)]
pub enum PlanningOutcome {
    Blocked {
        intent: OptimizationIntent,
        reasons: Vec<Reason>,
    },
    Candidate {
        plan: EffectivePlan,
        intent: OptimizationIntent,
        reasons: Vec<Reason>,
    },
}

enum_type!(PreviewQualification { Unvalidated });
enum_type!(PreviewFit { Unknown });
enum_type!(ArtifactIdentityScope {
    CompleteBundle,
    SelectedExecutable
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreviewArtifactIdentity {
    pub scope: ArtifactIdentityScope,
    pub sha256: String,
}
/// Settings advice only. Cannot be passed to revalidate or execution admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SettingsPreview {
    pub inventory_generation: String,
    pub config_revision: u64,
    pub adapter_id: String,
    pub adapter_bundle_sha256: Option<String>,
    pub identity: PreviewArtifactIdentity,
    pub model_bundle_sha256: String,
    pub backend: Backend,
    pub device_id: String,
    pub settings: EffectiveSettings,
    pub qualification: PreviewQualification,
    pub fit: PreviewFit,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
// As for PlanningOutcome, this is a single bounded return value, not a stored
// collection. Preserve direct owned fields in the public integration API.
#[allow(clippy::large_enum_variant)]
pub enum PreviewOutcome {
    Blocked {
        intent: OptimizationIntent,
        reasons: Vec<Reason>,
    },
    Preview {
        preview: SettingsPreview,
        intent: OptimizationIntent,
        reasons: Vec<Reason>,
    },
}
