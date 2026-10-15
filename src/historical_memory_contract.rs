//! Data-only contract shared by the backend and native adapter. These values
//! never grant recall, model, agent, scheduler, or tool authority.
use serde::{Deserialize, Serialize};

pub const PAGE_SIZE: usize = 32;
pub const MAX_SCAN_ROWS: usize = 256;
pub const MAX_ROW_BYTES: usize = 256 * 1024;
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_NATIVE_PAGES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalKind {
    Episode,
    Lesson,
}
impl HistoricalKind {
    pub fn table(self) -> &'static str {
        match self {
            Self::Episode => "memory_episodes",
            Self::Lesson => "lessons",
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Episode => "episode",
            Self::Lesson => "lesson",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentScope {
    All,
    Global,
    Agent { id: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalFilter {
    pub kind: HistoricalKind,
    pub scope: AgentScope,
    pub session_id: Option<String>,
    pub query: String,
    pub trash: bool,
}
impl Default for HistoricalFilter {
    fn default() -> Self {
        Self {
            kind: HistoricalKind::Episode,
            scope: AgentScope::All,
            session_id: None,
            query: String::new(),
            trash: false,
        }
    }
}
pub fn identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
pub fn lower_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn request_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}
impl HistoricalFilter {
    pub fn valid(&self) -> bool {
        self.query.len() <= 1024
            && self.session_id.as_deref().is_none_or(identifier)
            && match &self.scope {
                AgentScope::Agent { id } => identifier(id),
                _ => true,
            }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExactValue {
    Null,
    Integer(String),
    RealBits(String),
    Text(String),
    TextBytes(String),
    Blob(String),
}
impl ExactValue {
    pub fn text(&self) -> Option<&str> {
        if let Self::Text(v) = self {
            Some(v)
        } else {
            None
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactRow {
    pub columns: Vec<String>,
    pub values: Vec<ExactValue>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalSource {
    pub import_id: String,
    pub source_schema: u32,
    pub table: String,
    pub row_number: i64,
    pub payload_sha256: String,
}
impl HistoricalSource {
    pub fn valid(&self, kind: HistoricalKind) -> bool {
        identifier(&self.import_id)
            && self.source_schema == 25
            && self.table == kind.table()
            && self.row_number >= 0
            && lower_hash(&self.payload_sha256)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalData {
    Episode {
        agent_id: Option<ExactValue>,
        session_id: Option<ExactValue>,
        summary: Option<ExactValue>,
        salience: Option<ExactValue>,
        original_pinned: Option<ExactValue>,
        created_at: Option<ExactValue>,
        valid_to: Option<ExactValue>,
    },
    Lesson {
        agent_id: Option<ExactValue>,
        source_session_id: Option<ExactValue>,
        polarity: Option<ExactValue>,
        trigger: Option<ExactValue>,
        root_cause: Option<ExactValue>,
        corrected_behavior: Option<ExactValue>,
        scope: Option<ExactValue>,
        usage_count: Option<ExactValue>,
        success_count: Option<ExactValue>,
        created_at: Option<ExactValue>,
        valid_to: Option<ExactValue>,
    },
}
impl HistoricalData {
    pub fn kind(&self) -> HistoricalKind {
        match self {
            Self::Episode { .. } => HistoricalKind::Episode,
            Self::Lesson { .. } => HistoricalKind::Lesson,
        }
    }
    pub fn summary(&self) -> Option<&str> {
        match self {
            Self::Episode { summary, .. } => summary.as_ref(),
            Self::Lesson { trigger, .. } => trigger.as_ref(),
        }
        .and_then(ExactValue::text)
    }
    pub fn agent_id(&self) -> Option<&ExactValue> {
        match self {
            Self::Episode { agent_id, .. } | Self::Lesson { agent_id, .. } => agent_id.as_ref(),
        }
    }
    pub fn session_id(&self) -> Option<&ExactValue> {
        match self {
            Self::Episode { session_id, .. } => session_id.as_ref(),
            Self::Lesson {
                source_session_id, ..
            } => source_session_id.as_ref(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingInfo {
    pub stored_kind: String,
    pub byte_len: Option<u64>,
    pub sha256: Option<String>,
    pub source_encoding: Option<String>,
    pub dimensions: Option<u64>,
    pub model_provenance: String,
    pub reuse_allowed: bool,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalOverlay {
    pub revision: u64,
    pub trashed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalRecord {
    pub source: HistoricalSource,
    pub original_id: Option<ExactValue>,
    pub data: HistoricalData,
    pub embedding: EmbeddingInfo,
    pub unknown_columns: Vec<String>,
    pub warnings: Vec<String>,
    pub overlay: HistoricalOverlay,
    pub runtime_activation: String,
}
impl HistoricalRecord {
    pub fn valid(&self) -> bool {
        self.source.valid(self.data.kind())
            && self.runtime_activation == "dormant"
            && !self.embedding.reuse_allowed
            && self.embedding.model_provenance == "unknown"
            && self.overlay.revision <= i64::MAX as u64
            && !(self.overlay.revision == 0 && self.overlay.trashed)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedIssue {
    pub row_number: i64,
    pub code: String,
    pub payload_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalPageReply {
    pub import_id: String,
    pub filter: HistoricalFilter,
    pub records: Vec<HistoricalRecord>,
    pub retained_issues: Vec<RetainedIssue>,
    pub next_cursor: Option<String>,
    pub scanned: usize,
    pub total_archived_rows: u64,
    pub runtime_activation: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalDetail {
    pub record: HistoricalRecord,
    pub original: ExactRow,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalAction {
    Trash,
    Restore,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalChange {
    pub source: HistoricalSource,
    pub expected_revision: u64,
    pub action: HistoricalAction,
    pub request_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalReceipt {
    pub source: HistoricalSource,
    pub request_id: String,
    pub action: HistoricalAction,
    pub overlay: HistoricalOverlay,
}
