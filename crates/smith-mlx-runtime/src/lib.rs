//! An explicitly selected MLX Swift worker, owned private pipes and Metal-only
//! execution. No downloads, Python, remote endpoints, or provider fallback.
mod bundle;
mod plan;
mod protocol;
mod supervisor;
pub use plan::{ApprovedPlan, LaunchPlan, ModelSelection, PlanSummary, SelectedWorker};
use serde::{Deserialize, Serialize};
pub use smith_managed_runtime::Cancellation;
use std::time::Duration;
pub use supervisor::{LaunchFailure, Operation, Runtime};

pub const FRAME_BYTES: usize = 131_072;
pub const OUTPUT_BYTES: usize = 524_288;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Native,
    CompatibleFixture,
}
pub type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InvalidSelection,
    SelectionChanged,
    ConsentMismatch,
    UnsupportedPlatform,
    InvalidRequest,
    UnsupportedCapability,
    Busy,
    StaleRevision,
    NotLoaded,
    RequestLimit,
    Cancelled,
    Deadline,
    Spawn,
    Io,
    Protocol,
    OutputLimit,
    ChildExited,
    Cleanup,
    Worker { code: String },
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Failure {}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Budgets {
    pub hello: Duration,
    pub operation: Duration,
    pub generation: Duration,
    pub drain: Duration,
    pub reap: Duration,
}
impl Default for Budgets {
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(5),
            operation: Duration::from_secs(180),
            generation: Duration::from_secs(120),
            drain: Duration::from_secs(5),
            reap: Duration::from_secs(2),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Template {
    #[serde(rename = "simple-chatml-v1")]
    SimpleChatml,
    #[serde(rename = "simple-granite-v1")]
    SimpleGranite,
    #[serde(rename = "qwen3-default-thinking-v1")]
    Qwen3DefaultThinking,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    RawCompletion,
    Chat,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    #[default]
    ModelDefault,
    Disabled,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WorkerFeatures {
    pub qwen3_thinking_control_v1: bool,
}
impl ThinkingMode {
    pub(crate) fn validate(self, manifest: &Manifest, features: WorkerFeatures) -> Result<()> {
        if manifest.template.is_none()
            || (self == Self::Disabled
                && (!features.qwen3_thinking_control_v1
                    || manifest.template != Some(Template::Qwen3DefaultThinking)))
        {
            return Err(Failure::UnsupportedCapability);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub model_type: String,
    pub files: Vec<FileRecord>,
    pub package_sha256: String,
    pub template: Option<Template>,
    pub context_limit: u32,
}
impl Manifest {
    pub fn capabilities(&self) -> Vec<Capability> {
        if self.template.is_some() {
            vec![Capability::RawCompletion, Capability::Chat]
        } else {
            vec![Capability::RawCompletion]
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}
#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}
#[derive(Clone, Debug)]
pub enum GenerationRequest {
    Raw {
        prompt: String,
        sampling: Sampling,
    },
    Chat {
        messages: Vec<Message>,
        sampling: Sampling,
    },
    /// Additive explicit control; existing Chat callers retain model-default behavior.
    ChatWithThinking {
        messages: Vec<Message>,
        sampling: Sampling,
        thinking_mode: ThinkingMode,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sampling {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub seed: Option<u64>,
}
impl Default for Sampling {
    fn default() -> Self {
        Self {
            max_tokens: 128,
            temperature: 0.6,
            top_p: 0.9,
            seed: None,
        }
    }
}
impl GenerationRequest {
    pub fn thinking_mode(&self) -> Option<ThinkingMode> {
        match self {
            Self::Raw { .. } => None,
            Self::Chat { .. } => Some(ThinkingMode::ModelDefault),
            Self::ChatWithThinking { thinking_mode, .. } => Some(*thinking_mode),
        }
    }
    pub(crate) fn wire_thinking_mode(&self) -> Option<ThinkingMode> {
        self.thinking_mode()
            .filter(|m| *m != ThinkingMode::ModelDefault)
    }
    pub(crate) fn sampling(&self) -> &Sampling {
        match self {
            Self::Raw { sampling, .. }
            | Self::Chat { sampling, .. }
            | Self::ChatWithThinking { sampling, .. } => sampling,
        }
    }
    pub(crate) fn validate(&self, manifest: &Manifest, features: WorkerFeatures) -> Result<()> {
        if let Some(mode) = self.thinking_mode() {
            mode.validate(manifest, features)?;
        }
        let (sampling, bytes, valid) = match self {
            Self::Raw { prompt, sampling } => (sampling, prompt.len(), !prompt.is_empty()),
            Self::Chat { messages, sampling }
            | Self::ChatWithThinking {
                messages, sampling, ..
            } => (
                sampling,
                messages.iter().map(|m| m.content.len()).sum(),
                !messages.is_empty() && messages.len() <= 64,
            ),
        };
        if !valid
            || bytes > 65536
            || !(1..=2048).contains(&sampling.max_tokens)
            || sampling.max_tokens > manifest.context_limit
            || !sampling.temperature.is_finite()
            || !(0.0..=2.0).contains(&sampling.temperature)
            || !sampling.top_p.is_finite()
            || sampling.top_p <= 0.0
            || sampling.top_p > 1.0
        {
            return Err(Failure::InvalidRequest);
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub enum Request {
    Inspect,
    Load,
    Generate(GenerationRequest),
    Unload,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Idle,
    Inspecting,
    Loading,
    Ready,
    Generating,
    Unloading,
    Stopping,
    Stopped,
    Failed,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub session_id: String,
    pub revision: u64,
    pub phase: Phase,
    pub pid: Option<u32>,
    pub manifest: Option<Manifest>,
    pub inspection: Option<Manifest>,
    pub failure: Option<Failure>,
    pub spawned: u64,
    pub reaped: u64,
    pub accepting: bool,
    pub backend: &'static str,
    pub execution_kind: ExecutionKind,
    pub worker_features: WorkerFeatures,
}
/// Captured request intent and exact runtime identity, without prompt text or local paths.
/// This is descriptive evidence, not a reusable execution grant or completion acknowledgement.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GenerationProvenance {
    pub runtime_plan_id: String,
    pub worker_sha256: String,
    pub runtime_bundle_sha256: String,
    pub package_sha256: String,
    pub session_id: String,
    pub runtime_revision: u64,
    pub capability: Capability,
    pub template: Option<Template>,
    pub thinking_mode: Option<ThinkingMode>,
    pub sampling: Sampling,
    pub worker_features: WorkerFeatures,
}
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub request_id: String,
    pub revision: u64,
    pub text: String,
    pub incomplete: bool,
    pub finished: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    Cancelled,
}
#[derive(Clone, Debug, Serialize)]
pub struct Completion {
    pub text: String,
    pub incomplete: bool,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: FinishReason,
    /// Present only when the worker's successful terminal confirmed an explicit option.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<ThinkingMode>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Manifest(Manifest),
    Completion(Completion),
    Unloaded,
}
