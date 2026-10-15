//! Explicitly consented native llama-server supervision. No discovery or downloads.
//! Read the integration/security contract in `docs/NATIVE-RUNTIME.md` before wiring.
mod gguf;
mod inventory;
mod plan;
mod settings;
mod startup;
mod startup_parse;
mod supervisor;
mod task_schema;
mod transport;

pub use gguf::{inspect_gguf, inspect_gguf_reader, GgufFailure, GgufMetadata, KvDimensions};
pub use inventory::{
    ApprovedRuntimeProbe, DeviceBackend, DeviceObservation, ProbeOutput, RuntimeProbe,
    RuntimeProbePlan, RuntimeProbeReport, RuntimeProbeSummary,
};
pub use settings::{AdvancedSettings, FlashAttention, KvType, RuntimeCompatibility};

pub use plan::{
    inspect_selected_artifact, ApprovedPlan, Artifact, ArtifactKind, BackendRequest, ExecutionKind,
    LoadConfig, LoadPlan, Manifest, PlanSummary, Protocol,
};
pub use startup::{
    ReportedBufferKind, ReportedModelBuffer, ReportedOffload, StartupDiagnosticLine,
    StartupDiagnostics, StartupIdentity, StartupObservation, StartupPlacement, StartupStream,
    StartupStreamSummary, StartupUnknownReason,
};
pub use supervisor::{Generation, Runtime};

use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;

/// Latched, cloneable cancellation. Cancellation is never an edge notification.
#[derive(Clone)]
pub struct Cancellation(watch::Sender<bool>);
impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}
impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InvalidManifest,
    InvalidConfig,
    ArtifactChanged,
    ConsentMismatch,
    UnsupportedPlatform,
    UnsupportedProtocol,
    UnsupportedCapability,
    StaleRevision,
    StaleGeneration,
    WrongModel,
    Busy,
    NotReady,
    Cancelled,
    ShuttingDown,
    Spawn,
    StartupTimeout,
    GenerationTimeout,
    ChildExited,
    EndpointOwnership,
    Transport,
    Protocol,
    ResponseTooLarge,
    Cleanup,
    Io,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Failure {}
pub type Result<T> = std::result::Result<T, Failure>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopped,
    Loading,
    Ready,
    Stopping,
    Failed,
    CleanupFailed,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Target {
    pub owner: Uuid,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    TextChat,
    Tools,
    Vision,
    Audio,
    Embeddings,
    Reasoning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Unknown,
    AdapterSupported,
    DeclaredByManifest,
    ProtocolObserved,
    Unsupported,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub target: Target,
    pub revision: u64,
    pub phase: Phase,
    pub model_id: Option<String>,
    pub plan_id: Option<String>,
    pub pid: Option<u32>,
    pub active_leases: usize,
    pub execution_kind: Option<ExecutionKind>,
    pub requested_backend: Option<BackendRequest>,
    /// Deliberately unknown: health/chat responses do not prove placement.
    pub observed_backend: Evidence,
    /// This adapter owns one direct process; descendant/crash cleanup is not certified.
    pub process_tree_cleanup: Evidence,
    pub parent_crash_cleanup: Evidence,
    pub text_chat: Evidence,
    pub failure: Option<Failure>,
    pub spawned: u64,
    pub reaped: u64,
    pub accepting: bool,
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
pub struct TextRequest {
    pub messages: Vec<Message>,
    pub max_tokens: u32,
}
impl TextRequest {
    pub(crate) fn body(&self, model: &str, format: task_schema::ResponseFormat) -> Result<Vec<u8>> {
        if self.messages.is_empty()
            || self.messages.len() > 256
            || self.max_tokens == 0
            || self.max_tokens > 32768
        {
            return Err(Failure::InvalidConfig);
        }
        let mut body = serde_json::json!({"model":model,
            "messages":self.messages,"max_tokens":self.max_tokens,"stream":false});
        format.apply(&mut body);
        let body = serde_json::to_vec(&body).map_err(|_| Failure::Protocol)?;
        if body.len() > 128 * 1024 {
            return Err(Failure::InvalidConfig);
        }
        Ok(body)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Completion {
    pub target: Target,
    pub run_id: Uuid,
    pub text: String,
    pub finish_reason: String,
    pub usage: Option<Usage>,
    pub execution_kind: ExecutionKind,
}

/// All deadlines are upper bounds for supervisor-controlled async work; a stuck OS
/// kill/reap is reported CleanupFailed, never a successful stop.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Budgets {
    pub startup: Duration,
    pub generation: Duration,
    pub reap: Duration,
}
impl Default for Budgets {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(120),
            generation: Duration::from_secs(120),
            reap: Duration::from_secs(2),
        }
    }
}
