//! Agent-domain foundation. Host application code creates grants; model text,
//! persisted traces, and tool arguments never manufacture authority.
//! See `docs/AGENT-TOOLS.md` for the implemented source map and deliberate gaps.
#![forbid(unsafe_code)]

mod authority;
mod error;
#[cfg(feature = "standalone-sqlite")]
pub mod fixture;
mod host;
mod instructions;
mod model_rejection;
mod plugin_import;
mod registry;
mod task;
mod trace;
mod workspace;

pub use authority::{
    Capability, ExecutionGrant, GrantManifest, Principal, ResourceKind, ResourceScope,
};
pub use error::{Error, Result};
pub use host::{arguments_sha256, HostToolAdapter, HostToolContext};
pub use instructions::{
    CapturedInstruction, CapturedInstructions, InstructionKind, InstructionRef,
    InstructionSelection,
};
pub use model_rejection::{ModelRejectionReason, ModelRejectionRecord, RejectedModelReply};
pub use plugin_import::{
    DeclarativePluginManifest, PluginFileEvidence, PluginSourceEvidence, SelectedPlugin,
};
pub use registry::{Availability, ToolCall, ToolDescriptor, ToolLimits, ToolRegistry};
pub use task::{
    AgentModelTarget, AgentPersona, ApprovalDecision, ModelReply, RecoveryDecision, Task,
    TaskEngine, TaskState, ToolOutcome,
};
#[cfg(feature = "standalone-sqlite")]
pub use trace::SqliteTraceStore;
pub use trace::{
    decode_task_record, decode_trace_event, encode_transition, MemoryTraceStore, TraceEvent,
    TraceRecord, TraceStore,
};
pub use workspace::{DirectoryEntry, EntryKind, Workspace, WorkspaceIdentity};
