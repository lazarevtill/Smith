//! Platform authority is owned by the authenticated host, never by tool text.
//! Constructing a backend performs no OS discovery, capture, input or prompts.
//! See docs/PLATFORM-ADAPTERS.md before connecting a native target picker.
mod broker;
pub mod fixture;
mod host;
pub mod native;

pub use broker::{ActionApproval, PlatformSession};
pub use host::PlatformToolAdapter;
use serde::{Deserialize, Serialize};
use std::{fmt, time::Duration};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Error {
    InvalidInput(String),
    Denied(String),
    Unsupported(String),
    PermissionRequired(String),
    TargetChanged,
    SnapshotExpired,
    SnapshotChanged,
    ApprovalMismatch,
    ApprovalConsumed,
    Cancelled,
    ShuttingDown,
    Backend(String),
    UncertainSideEffect(String),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Support {
    Available,
    RequiresUserPermission(String),
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlatformCapabilities {
    pub platform: String,
    pub execution_kind: ExecutionKind,
    pub system_monitor: Support,
    pub screen_elements: Support,
    pub screen_click: Support,
    pub screen_type: Support,
    pub screenshot: Support,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Native,
    SyntheticFixture,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TargetIdentity {
    pub id: String,
    pub label: String,
    pub execution_kind: ExecutionKind,
}

/// Host-only selection. This type is deliberately not deserializable. Native
/// selection may read only the explicitly chosen process's focused window.
pub enum TargetRequest {
    SyntheticFixture,
    MacProcess { pid: i32 },
    MacProcessIdentity { pid: i32, process_identity: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProcessTarget {
    pub id: String,
    pub pid: i32,
    pub label: String,
}

/// Explicit application-user grants. OS permission alone grants nothing here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TargetPermissions {
    pub read_elements: bool,
    pub interact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Bounds {
    pub fn contains(self, x: i32, y: i32) -> bool {
        i64::from(x) >= i64::from(self.x)
            && i64::from(y) >= i64::from(self.y)
            && i64::from(x) < i64::from(self.x) + i64::from(self.width)
            && i64::from(y) < i64::from(self.y) + i64::from(self.height)
    }
    pub fn center(self) -> (i32, i32) {
        (
            (i64::from(self.x) + i64::from(self.width) / 2)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            (i64::from(self.y) + i64::from(self.height) / 2)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UiElement {
    pub id: String,
    pub role: String,
    pub name: Option<String>,
    pub bounds: Bounds,
    pub enabled: bool,
    pub focused: bool,
    pub can_press: bool,
    pub can_insert_text: bool,
    pub text_selection: Option<TextSelection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TextSelection {
    pub location: u64,
    pub length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Frame {
    pub target: TargetIdentity,
    pub bounds: Bounds,
    pub elements: Vec<UiElement>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub id: String,
    pub generation: u64,
    pub valid_for_ms: u64,
    pub frame: Frame,
    /// Source-compatible compact element listing; this is untrusted UI data.
    pub elements: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Click { x: i32, y: i32 },
    TypeText { text: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionPlan {
    pub id: String,
    pub binding: String,
    pub snapshot_id: String,
    pub target: TargetIdentity,
    pub element: UiElement,
    pub action: Action,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionReceipt {
    pub ok: bool,
    pub plan_id: String,
    pub target: TargetIdentity,
    pub execution_kind: ExecutionKind,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiskUsage {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SystemSnapshot {
    pub cpu_percent: f64,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub disks: Vec<DiskUsage>,
    pub execution_kind: ExecutionKind,
    pub sample_ms: u64,
}

/// Backend methods are trusted implementation hooks. The host must expose only
/// PlatformSession to requests. Native implementations must scope every action
/// to retained target/element handles and never fall back to global OS input.
pub trait PlatformBackend: Send {
    fn capabilities(&self) -> PlatformCapabilities;
    fn select(&mut self, request: TargetRequest) -> Result<TargetIdentity>;
    fn frame(&mut self, target: &TargetIdentity) -> Result<Frame>;
    fn act(&mut self, plan: &ActionPlan) -> Result<()>;
    fn system_monitor(&mut self) -> Result<SystemSnapshot>;
    fn release(&mut self);
}

pub trait Clock: Send + Sync {
    fn elapsed(&self) -> Duration;
}
pub struct MonotonicClock(std::time::Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(std::time::Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}
