//! Inert diagnostics for rejected model output. These records are never parsed
//! as tool calls or used as model input by the host runner.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// Match the largest bounded transport used by the host (managed runtime).
// Persisted text and its encoded envelope remain independently much smaller.
const MAX_SOURCE: usize = 2 * 1024 * 1024;
const MAX_TEXT: usize = 16 * 1024;
pub(crate) const MAX_REJECTION: usize = 20 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRejectionReason {
    InvalidStructuredStep,
    UnavailableToolOrArguments,
    Incomplete,
    ResponseLimit,
    InvalidTaskReply,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectedModelReply {
    pub reason: ModelRejectionReason,
    pub text: String,
    pub original_bytes: u64,
    pub sha256: String,
    pub truncated: bool,
    pub provenance: Value,
}
impl RejectedModelReply {
    pub fn capture(reason: ModelRejectionReason, text: &str, provenance: Value) -> Result<Self> {
        if text.len() > MAX_SOURCE || provenance.to_string().len() > 4096 {
            return Err(Error::LimitExceeded(
                "Rejected model response or provenance".into(),
            ));
        }
        let mut reply = Self {
            reason,
            text: String::new(),
            original_bytes: text.len() as u64,
            sha256: crate::workspace::sha256(text),
            truncated: !text.is_empty(),
            provenance,
        };
        let ends = std::iter::once(0)
            .chain(
                text.char_indices()
                    .map(|(offset, c)| offset + c.len_utf8())
                    .take_while(|end| *end <= MAX_TEXT),
            )
            .collect::<Vec<_>>();
        let (mut low, mut high) = (0, ends.len());
        while low + 1 < high {
            let middle = (low + high) / 2;
            reply.text = text[..ends[middle]].into();
            reply.truncated = ends[middle] < text.len();
            if reply.encoded_len()? <= MAX_REJECTION {
                low = middle;
            } else {
                high = middle;
            }
        }
        reply.text = text[..ends[low]].into();
        reply.truncated = ends[low] < text.len();
        reply.validate()?;
        Ok(reply)
    }
    pub(crate) fn encoded_len(&self) -> Result<usize> {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .map_err(|_| Error::InvalidInput("Rejected model response encoding".into()))
    }
    pub fn validate(&self) -> Result<()> {
        if self.text.len() > MAX_TEXT
            || self.original_bytes > MAX_SOURCE as u64
            || self.provenance.to_string().len() > 4096
            || self.encoded_len()? > MAX_REJECTION
        {
            return Err(Error::LimitExceeded(
                "Rejected model response evidence".into(),
            ));
        }
        if self.original_bytes < self.text.len() as u64
            || self.truncated != (self.original_bytes > self.text.len() as u64)
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || (!self.truncated && self.sha256 != crate::workspace::sha256(&self.text))
        {
            return Err(Error::InvalidInput(
                "Rejected model response evidence is malformed".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRejectionRecord {
    pub step_id: String,
    pub reply: RejectedModelReply,
}
impl ModelRejectionRecord {
    pub(crate) fn validate(&self) -> Result<()> {
        if !uuid::Uuid::parse_str(&self.step_id).is_ok_and(|id| id.to_string() == self.step_id) {
            return Err(Error::InvalidInput("Rejected model step identity".into()));
        }
        self.reply.validate()
    }
}
