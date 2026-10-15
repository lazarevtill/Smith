//! Export the reviewed saved branch, never its presentation or provider context.
//! JSON preserves the active message objects supplied by the current API; it is
//! not the original Smith Message wire schema or an archive of every branch.
use crate::bridge::{encode_component, Bridge, Chat, Message, Result, Transport};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_MESSAGES: usize = 10_000;

#[derive(Clone)]
struct DisplayedBranch {
    leaf: Option<String>,
    messages: Vec<Message>,
}
/// Minted only from a worker-owned history row. Clones retain the originating
/// session and share the single write attempt; callers cannot deserialize it.
#[derive(Clone)]
pub struct ConversationExportReview {
    session: String,
    chat: Chat,
    displayed: Option<DisplayedBranch>,
    used: Arc<AtomicBool>,
}
impl ConversationExportReview {
    pub fn chat(&self) -> &Chat {
        &self.chat
    }
    pub fn matches(&self, row: &Chat) -> bool {
        self.chat.id == row.id && self.chat.version == row.version
    }
}

impl<T: Transport> Bridge<T> {
    /// Capture alongside the displayed list. An unloaded row binds its saved
    /// version; the currently open row also binds its exact visible branch.
    pub fn conversation_export_reviews(&self) -> BTreeMap<String, ConversationExportReview> {
        let Some(session) = self.session.as_ref().filter(|_| self.view.available) else {
            return BTreeMap::new();
        };
        self.view
            .chats
            .iter()
            .filter_map(|chat| {
                if chat.id.is_empty() || chat.id.len() > 4096 || chat.version < 0 {
                    return None;
                }
                let (chat, displayed) = match self
                    .view
                    .conversation
                    .as_ref()
                    .filter(|c| c.chat.id == chat.id)
                {
                    Some(c) => {
                        if c.chat.version < 0
                            || c.active_messages.is_empty()
                            || c.active_messages
                                .iter()
                                .any(|m| !m.persisted || !terminal(&m.status))
                        {
                            return None;
                        }
                        (
                            &c.chat,
                            Some(DisplayedBranch {
                                leaf: c.active_leaf.clone(),
                                messages: c.active_messages.clone(),
                            }),
                        )
                    }
                    None => (chat, None),
                };
                Some((
                    chat.id.clone(),
                    ConversationExportReview {
                        session: session.clone(),
                        chat: chat.clone(),
                        displayed,
                        used: Arc::new(AtomicBool::new(false)),
                    },
                ))
            })
            .collect()
    }

    pub fn export_conversation(
        &mut self,
        review: &ConversationExportReview,
        destination: &Path,
    ) -> Result<()> {
        let as_json = match destination.extension().and_then(|s| s.to_str()) {
            Some(s) if s.eq_ignore_ascii_case("json") => true,
            Some(s) if s.eq_ignore_ascii_case("md") => false,
            _ => {
                return Err(
                    "Choose a new .md or .json filename for the conversation export.".into(),
                )
            }
        };
        if !destination.is_absolute() {
            return Err("Choose an absolute local export destination.".into());
        }
        if review.used.swap(true, Ordering::AcqRel) {
            return Err(
                "This export attempt was already used. Select Export again for a new attempt."
                    .into(),
            );
        }
        self.check_session()?;
        if self.session.as_deref() != Some(review.session.as_str()) {
            return Err("This export belongs to a previous workspace.".into());
        }
        if review.displayed.is_none() && !self.view.chats.iter().any(|c| review.matches(c)) {
            return Err("The conversation changed. Refresh it before exporting.".into());
        }
        if let Some(displayed) = &review.displayed {
            let current = self
                .view
                .conversation
                .as_ref()
                .ok_or("The displayed conversation changed before export.")?;
            if !review.matches(&current.chat)
                || current.active_leaf != displayed.leaf
                || current.active_messages != displayed.messages
            {
                return Err("The displayed branch changed. Select Export again.".into());
            }
        }
        let snapshot = self.get(&format!("/api/chats/{}", encode_component(&review.chat.id)))?;
        // Also enforce bounds for non-HTTP transports used in qualification.
        serde_json::to_writer(
            LimitWriter {
                remaining: MAX_SNAPSHOT_BYTES,
            },
            &snapshot,
        )
        .map_err(|_| {
            "This conversation exceeds the bounded 2 MiB export snapshot. No file was written."
        })?;
        let messages = validated_branch(&snapshot, review)?;
        let bytes = if as_json {
            let mut out = BoundedBytes::default();
            serde_json::to_writer_pretty(&mut out, messages)
                .map_err(|_| "The JSON export exceeds 8 MiB. No file was written.")?;
            out.0
        } else {
            let mut out = BoundedBytes::default();
            out.write_all(b"# Conversation\n\n")
                .map_err(|_| "Export is too large.")?;
            for message in messages {
                let status = message["status"].as_str().unwrap();
                if status != "completed" {
                    write!(&mut out, "> Reply {status}\n\n").map_err(|_| "Export is too large.")?;
                }
                // Validation below makes both strings safe to access. Content
                // is copied literally, including reasoning markers and code.
                write!(
                    &mut out,
                    "**{}:**\n\n{}\n\n---\n\n",
                    message["role"].as_str().unwrap(),
                    message["content"].as_str().unwrap()
                )
                .map_err(|_| "The Markdown export exceeds 8 MiB. No file was written.")?;
            }
            out.0
        };
        // A revoked response must not cause a plaintext write in the old
        // workspace. The captured immutable snapshot is the exported version.
        self.check_session()?;
        crate::documents::write_new(destination, &bytes)?;
        self.view.status = "Conversation exported.".into();
        Ok(())
    }
}

fn terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "limited" | "failed" | "cancelled" | "interrupted" | "imported_reference"
    )
}
fn validated_branch<'a>(
    snapshot: &'a Value,
    review: &ConversationExportReview,
) -> Result<&'a Vec<Value>> {
    if snapshot["chat"]["id"].as_str() != Some(&review.chat.id)
        || snapshot["chat"]["version"].as_i64() != Some(review.chat.version)
    {
        return Err("The saved conversation changed. Refresh it before exporting.".into());
    }
    let all = snapshot["messages"]
        .as_array()
        .ok_or("Export snapshot lacks saved history.")?;
    let active = snapshot["active_messages"]
        .as_array()
        .ok_or("Export snapshot lacks the selected branch.")?;
    if active.is_empty() {
        return Err("This conversation has no saved messages to export.".into());
    }
    if all.len() > MAX_MESSAGES || active.len() > MAX_MESSAGES {
        return Err("The conversation exceeds the 10,000-message export bound.".into());
    }
    let mut by_id = BTreeMap::new();
    for message in all {
        let id = message["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 4096)
            .ok_or("Export snapshot has an invalid message identity.")?;
        if by_id.insert(id, message).is_some() {
            return Err("Export snapshot has duplicate message identities.".into());
        }
    }
    let leaf = snapshot["active_leaf"]
        .as_str()
        .ok_or("Export snapshot lacks its active leaf.")?;
    if active.last().and_then(|m| m["id"].as_str()) != Some(leaf)
        || review
            .displayed
            .as_ref()
            .is_some_and(|d| d.leaf.as_deref() != Some(leaf))
    {
        return Err("The selected branch changed before export.".into());
    }
    let mut previous = None;
    let mut seen = BTreeSet::new();
    for message in active {
        let id = message["id"]
            .as_str()
            .ok_or("Export message identity is missing.")?;
        if !seen.insert(id) {
            return Err("Export branch contains a repeated message.".into());
        }
        let saved = by_id
            .get(id)
            .ok_or("Export branch is missing a saved message.")?;
        let mut base = message
            .as_object()
            .ok_or("Export message is invalid.")?
            .clone();
        base.remove("siblings");
        base.remove("branch_index");
        let mut original = saved
            .as_object()
            .ok_or("Saved message is invalid.")?
            .clone();
        original.remove("siblings");
        original.remove("branch_index");
        if base != original {
            return Err("Active and saved message contents disagree; no file was written.".into());
        }
        let parent = message
            .get("parent_id")
            .ok_or("Export branch parent is missing.")?;
        if !(parent.is_null() && previous.is_none()
            || parent.as_str().is_some() && parent.as_str() == previous)
        {
            return Err("Export branch is incomplete or has an invalid parent.".into());
        }
        if !matches!(
            message["role"].as_str(),
            Some("user" | "assistant" | "system" | "tool")
        ) || message["content"].as_str().is_none()
            || !message["status"].as_str().is_some_and(terminal)
            || message.get("persisted").is_some_and(|v| v != true)
        {
            return Err("Export requires saved terminal messages; wait for saving or reload the conversation.".into());
        }
        previous = Some(id);
    }
    if let Some(displayed) = &review.displayed {
        if active.len() != displayed.messages.len()
            || active
                .iter()
                .zip(&displayed.messages)
                .any(|(raw, visible)| {
                    raw["id"] != visible.id
                        || raw["content"] != visible.content
                        || raw["role"] != visible.role
                        || raw["status"] != visible.status
                })
        {
            return Err("The saved branch no longer matches the displayed text. Reload it before exporting.".into());
        }
    }
    Ok(active)
}

struct LimitWriter {
    remaining: usize,
}
impl Write for LimitWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("export limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(Default)]
struct BoundedBytes(Vec<u8>);
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > crate::documents::MAX_FILE.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("export limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
