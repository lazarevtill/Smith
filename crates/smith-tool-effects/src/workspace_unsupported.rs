//! Explicit non-Unix refusal, not a host process fallback.
use crate::{Error, GitAction, PlanSummary, Result};
use serde_json::{json, Value};
use std::path::Path;
pub struct ExecutableGrant;
impl ExecutableGrant {
    pub fn inspect(_: impl Into<String>, _: &Path) -> Result<Self> {
        Err(unsupported())
    }
    pub fn inspect_with_prefix(_: impl Into<String>, _: &Path, _: Vec<String>) -> Result<Self> {
        Err(unsupported())
    }
    pub fn summary(&self) -> Value {
        json!({"unsupported":"Unix workspace adapter required"})
    }
}
pub struct WorkspaceGrant;
impl WorkspaceGrant {
    pub fn temporary() -> Result<Self> {
        Err(unsupported())
    }
    pub fn owned_temporary(_: &Path, _: Option<(u64, u64)>) -> Result<Self> {
        Err(unsupported())
    }
    pub fn host_path(&self) -> &Path {
        Path::new("")
    }
}
pub(crate) struct WorkspaceBinding;
impl WorkspaceBinding {
    pub fn new(
        _: WorkspaceGrant,
        _: Vec<ExecutableGrant>,
        _: Option<ExecutableGrant>,
    ) -> Result<Self> {
        Err(unsupported())
    }
    pub fn policy(&self) -> Value {
        json!({"unsupported":"Unix workspace adapter required"})
    }
    pub fn digest(&self) -> Result<String> {
        Err(unsupported())
    }
    pub fn verify_executables(&self) -> Result<()> {
        Err(unsupported())
    }
    pub fn resolve_command(&self, _: &str) -> Result<String> {
        Err(unsupported())
    }
    pub fn require_git(&self) -> Result<()> {
        Err(unsupported())
    }
    pub fn require_repository(&self) -> Result<()> {
        Err(unsupported())
    }
    pub fn shell(
        &self,
        _: &str,
        _: &[String],
        _: &PlanSummary,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        Err(unsupported())
    }
    pub fn git(
        &self,
        _: GitAction,
        _: &PlanSummary,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        Err(unsupported())
    }
    pub fn clone_from(
        &self,
        _: &Self,
        _: &PlanSummary,
        _: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Value> {
        Err(unsupported())
    }
}
pub(crate) fn relative(_: &str) -> Result<()> {
    Err(unsupported())
}
fn unsupported() -> Error {
    Error::Unsupported("owned process workspace requires Unix; no host fallback")
}
