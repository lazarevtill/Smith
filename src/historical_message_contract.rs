//! Data-only historical transcript cards. No transport or execution authority.
use crate::historical_memory_contract as archive;
pub use archive::{ExactRow, ExactValue};
use serde::{Deserialize, Serialize};

pub const MAX_ARCHIVE_ROW: usize = 16 * 1024 * 1024;
pub const MAX_IMAGE_FIELD: usize = 12 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGES: usize = 32;
pub const MAX_CALL_FIELD: usize = 512 * 1024;
pub const MAX_CALLS: usize = 128;
pub const MAX_ARGUMENT_PREVIEW: usize = 32 * 1024;
pub const MAX_CARD_BYTES: usize = 512 * 1024;
pub const MAX_ANCESTORS: usize = 4096;
pub const MAX_ANCESTOR_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSource {
    pub import_id: String,
    pub source_schema: u32,
    pub table: String,
    pub row_number: i64,
    pub payload_sha256: String,
}
impl MessageSource {
    pub fn valid(&self) -> bool {
        archive::identifier(&self.import_id)
            && self.source_schema == 25
            && self.table == "messages"
            && self.row_number >= 0
            && archive::lower_hash(&self.payload_sha256)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    MissingField,
    InvalidFieldType,
    InvalidJson,
    InvalidEntry,
    DuplicateCallField,
    OversizedField,
    TooManyEntries,
    UnsupportedScheme,
    UnsupportedDataEncoding,
    UnsupportedImageFormat,
    InvalidBase64,
    ImageTooLarge,
    InvalidImage,
    MimeMismatch,
    InvalidIdentity,
    MissingAncestor,
    DuplicateIdentity,
    CrossSessionAncestor,
    NonEarlierAncestor,
    Cycle,
    AncestryLimit,
    IncompleteCalls,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedIssue {
    pub field: String,
    pub index: Option<usize>,
    pub reason: Reason,
    pub retained_bytes: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageState {
    Embedded {
        mime: String,
        decoded_bytes: u64,
        decoded_sha256: String,
        width: u32,
        height: u32,
    },
    RemoteUnresolved {
        scheme: String,
    },
    Retained {
        reason: Reason,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageReference {
    pub index: usize,
    pub source_string_bytes: Option<u64>,
    pub source_string_sha256: Option<String>,
    pub state: ImageState,
    pub fetched: NotFetched,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextPreview {
    pub text: String,
    pub original_bytes: u64,
    pub original_sha256: String,
    pub truncated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallCard {
    pub index: usize,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<TextPreview>,
    pub arguments_valid_json: Option<bool>,
    pub unknown_fields: Vec<String>,
    pub issue: Option<Reason>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallLocation {
    pub message: MessageSource,
    pub original_message_id: String,
    pub call_index: usize,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultLink {
    NotToolResult,
    MissingId,
    Unmatched { call_id: String },
    Unique { call_id: String, call: CallLocation },
    Ambiguous { call_id: String, candidates: usize },
    Unresolved { reason: Reason },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageCards {
    pub source: MessageSource,
    pub original_id: Option<ExactValue>,
    pub session_id: Option<ExactValue>,
    pub role: Option<ExactValue>,
    pub original_status: Option<ExactValue>,
    pub original_seq: Option<ExactValue>,
    pub parent_id: Option<ExactValue>,
    pub tool_call_id: Option<ExactValue>,
    pub images: Vec<ImageReference>,
    pub image_count: Option<usize>,
    pub calls: Vec<ToolCallCard>,
    pub call_count: Option<usize>,
    pub result_link: ResultLink,
    pub issues: Vec<RetainedIssue>,
    pub unknown_columns: Vec<String>,
    pub runtime_activation: Dormant,
}

/// This read-only contract cannot represent activation or a completed fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dormant {
    Dormant,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotFetched;
impl Serialize for NotFetched {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(false)
    }
}
impl<'de> Deserialize<'de> for NotFetched {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Err(serde::de::Error::custom(
                "historical references cannot be fetched",
            ))
        } else {
            Ok(Self)
        }
    }
}
