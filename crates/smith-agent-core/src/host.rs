//! Trusted host adapters. They are never constructed from model text or a trace.
use crate::{ExecutionGrant, Result, ToolCall, ToolDescriptor};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn arguments_sha256(arguments: &Value) -> String {
    Sha256::digest(arguments.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub struct HostToolContext<'a> {
    pub task_id: &'a str,
    pub started_revision: u64,
    pub call: &'a ToolCall,
    pub grant: &'a ExecutionGrant,
}

/// Attach only real, policy-bound adapters. Validation must strictly parse
/// arguments and check exact resource scopes without creating effects. Execute
/// must enforce cancellation before effect commit. DB effects should atomically
/// recheck the in-flight task revision and store an idempotency receipt. External
/// effects with unknown completion must return UncertainSideEffect. Approval,
/// trace and bounded task execution are supplied by TaskEngine, not by models.
pub trait HostToolAdapter: Send + Sync {
    fn descriptors(&self) -> Vec<ToolDescriptor>;
    fn validate(&self, grant: &ExecutionGrant, call: &ToolCall) -> Result<()>;
    fn execute(
        &self,
        context: &HostToolContext<'_>,
        check_active: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value>;
}
