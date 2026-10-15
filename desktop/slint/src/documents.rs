//! Selected files stay outside the prompt until their exact summary is reviewed.
//! Original bytes live in the keyed backend; native previews remain in memory.
use crate::bridge::{encode_component, Bridge, Chat, Result, Transport};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};
pub const MAX_FILE: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, Default)]
pub struct Documents {
    pub pending: Value,
    pub draft_identity: u64,
    pub preview_id: String,
    pub preview_title: String,
    pub preview_text: String,
    pub preview_path: String,
}
#[derive(Clone, Debug)]
pub struct DocumentReview {
    session: String,
    draft_identity: u64,
    owner: String,
    document: Value,
}
#[derive(Clone, Debug)]
pub struct DownloadReview {
    session: String,
    pub name: String,
    path: String,
}
#[derive(Clone, Debug)]
pub struct AnswerReview {
    session: String,
    chat: Chat,
    content: String,
}
fn artifact_download_path(artifact: &Value, id: &str) -> Result<String> {
    let projected = format!("/api/media/artifacts/{}/original", encode_component(id));
    let Some(original) = artifact.get("original_import").filter(|v| !v.is_null()) else {
        return Ok(projected);
    };
    if !matches!(original["source_schema"].as_u64(), Some(25 | 26)) {
        return Err("This imported artifact has an unsupported source identity.".into());
    }
    if original["content_kind"] != "managed_original" || original["download_url"].is_null() {
        return Ok(String::new());
    }
    let expected = format!(
        "/api/legacy-import/artifacts/{}/bytes",
        encode_component(id)
    );
    if original["download_url"].as_str() != Some(expected.as_str())
        || original["byte_len"].as_u64().is_none()
        || !original["sha256"]
            .as_str()
            .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("The imported artifact download does not match the selected original.".into());
    }
    if original["byte_len"].as_u64().unwrap() > MAX_FILE as u64 {
        return Ok(String::new());
    }
    Ok(expected)
}
pub(crate) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "Secure local request identity unavailable.")?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
/// No-follow descriptor custody rejects links/FIFOs and checks bounded contents.
/// A writer modifying this selected file is detected by descriptor metadata;
/// the server independently validates the exact uploaded bytes and hash.
pub fn read_selected(path: &Path) -> Result<(String, Vec<u8>)> {
    if !path.is_absolute() {
        return Err("Choose an absolute local file path.".into());
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|n| !n.is_empty() && n.len() <= 240 && !n.chars().any(char::is_control))
        .ok_or("Unsupported selected filename.")?
        .to_owned();
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "Selected file could not be opened safely.")?;
    let before = file
        .metadata()
        .map_err(|_| "Selected file metadata unavailable.")?;
    if !before.is_file() || before.len() > MAX_FILE as u64 {
        return Err("Select a regular file no larger than 8 MiB.".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take((MAX_FILE + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "Selected file read failed.")?;
    let after = file
        .metadata()
        .map_err(|_| "Selected file metadata changed.")?;
    if bytes.len() > MAX_FILE
        || after.len() != before.len()
        || bytes.len() as u64 != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err("Selected file changed while being read. Select it again.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err("Selected file identity changed.".into());
        }
    }
    Ok((name, bytes))
}
/// Export only to an explicitly selected *new* destination. Existing files,
/// including symlinks, are refused; no implicit replace or temporary plaintext.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if !path.is_absolute() || bytes.len() > MAX_FILE {
        return Err("Invalid export destination or size.".into());
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "Export needs a new filename; existing files are not overwritten.")?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        return Err(format!(
            "Export write failed; a partial file may remain at the selected destination: {error}"
        ));
    }
    Ok(())
}
impl<T: Transport> Bridge<T> {
    pub fn inspect_document(&mut self, identity: u64, path: &Path) -> Result<()> {
        self.check_session()?;
        if identity != self.view.draft.identity {
            return Err("Selected file belongs to a previous composer.".into());
        }
        let (name, bytes) = read_selected(path)?;
        let session = self.session.clone().ok_or("Workspace unavailable.")?;
        let metadata =
            json!({"request_id":random_id()?,"filename":name,"declared_mime":null,"metadata":{}});
        let document = self.transport.upload(&session, metadata, &bytes)?;
        self.check_session()?;
        if document["id"].as_str().is_none() || document["allowed_delivery"].as_array().is_none() {
            return Err("Malformed document review.".into());
        }
        self.view.documents.pending = document;
        self.view.documents.draft_identity = identity;
        self.refresh_actions()?;
        Ok(())
    }
    pub fn review_saved_document(&mut self, identity: u64, id: &str) -> Result<()> {
        self.check_session()?;
        if identity != self.view.draft.identity {
            return Err("Document belongs to a previous composer.".into());
        }
        let document = self.get(&format!("/api/media/{}", encode_component(id)))?;
        self.check_session()?;
        if document["id"] != id {
            return Err("Document identity mismatch.".into());
        }
        self.view.documents.pending = document;
        self.view.documents.draft_identity = identity;
        Ok(())
    }
    pub fn document_review(&self) -> Result<DocumentReview> {
        if self.view.documents.pending["id"].as_str().is_none()
            || self.view.documents.draft_identity != self.view.draft.identity
        {
            return Err("Select a document for this composer first.".into());
        }
        Ok(DocumentReview {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            draft_identity: self.view.draft.identity,
            owner: self.view.draft.owner.clone(),
            document: self.view.documents.pending.clone(),
        })
    }
    pub fn attach_document(&mut self, review: &DocumentReview, delivery: &str) -> Result<()> {
        self.check_session()?;
        if self.session.as_deref() != Some(&review.session)
            || review.draft_identity != self.view.draft.identity
            || review.owner != self.view.draft.owner
            || review.document != self.view.documents.pending
        {
            return Err("Document approval is stale. Review the current file and composer.".into());
        }
        if !review.document["allowed_delivery"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d == delivery))
        {
            return Err("This document delivery is unsupported. Images require explicit reference-only approval.".into());
        }
        if self
            .view
            .draft
            .media
            .iter()
            .any(|v| v["id"] == review.document["id"])
        {
            return Err("This document is already attached.".into());
        }
        if self.view.draft.media.len() + self.view.draft.attachments.len() >= 3 {
            return Err("Attach at most three files per message.".into());
        }
        self.view
            .draft
            .media
            .push(json!({"id":review.document["id"],"delivery":delivery}));
        self.view.draft.dirty = true;
        self.save_draft()?;
        self.view.documents.pending = Value::Null;
        Ok(())
    }
    pub fn promote_document(&mut self, review: &DocumentReview) -> Result<()> {
        self.check_session()?;
        if self.session.as_deref() != Some(&review.session)
            || review.document != self.view.documents.pending
        {
            return Err("The reviewed import changed. Review it again before saving.".into());
        }
        let id = review.document["id"]
            .as_str()
            .ok_or("Imported document identity missing.")?;
        let result = self.transport.request(
            "POST",
            &format!("/api/media/{}/artifact", encode_component(id)),
            self.session.as_deref(),
            Some(json!({})),
        )?;
        self.check_session()?;
        self.refresh_lists()?;
        self.view.documents.pending = Value::Null;
        self.open_artifact(
            result["artifact"]["id"]
                .as_str()
                .ok_or("Saved artifact identity missing.")?,
        )
    }
    pub fn clear_documents(&mut self, identity: u64) -> Result<()> {
        self.check_session()?;
        if identity != self.view.draft.identity {
            return Err("Attachment action belongs to an earlier composer.".into());
        }
        self.view.draft.media.clear();
        self.view.draft.attachments.clear();
        self.view.draft.dirty = true;
        self.save_draft()
    }
    pub fn open_artifact(&mut self, id: &str) -> Result<()> {
        self.check_session()?;
        let artifact = self
            .view
            .artifacts
            .iter()
            .find(|v| v["id"] == id)
            .cloned()
            .ok_or("Refresh artifacts before opening this item.")?;
        let path = artifact_download_path(&artifact, id)?;
        let projected_path = format!("/api/media/artifacts/{}/original", encode_component(id));
        let mut text = if let Some(media) = artifact["document"]["id"].as_str() {
            let document = self.get(&format!("/api/media/{}", encode_component(media)))?;
            if document["content"]["kind"] == "document" {
                format!(
                    "{}{}",
                    if document["content"]["truncated"] == true {
                        "Bounded excerpt; the original is retained for download.\n\n"
                    } else {
                        ""
                    },
                    document["content"]["context"].as_str().unwrap_or("")
                )
            } else {
                format!(
                    "Image reference only. No image pixels are sent to the text model.\n{}",
                    serde_json::to_string_pretty(&document).unwrap()
                )
            }
        } else {
            String::from_utf8(self.transport.download(
                &projected_path,
                self.session.as_deref().ok_or("Workspace unavailable.")?,
            )?)
            .map_err(|_| "This artifact is not UTF-8 text.")?
        };
        if artifact
            .get("original_import")
            .is_some_and(|v| !v.is_null())
        {
            text = format!(
                "{}\n\n{}",
                if path.is_empty() {
                    "Imported preview. Exact bytes are unavailable here or exceed the native 8 MiB export limit; retained encrypted import data is unchanged."
                } else {
                    "Imported preview. Export saves the authenticated original bytes."
                },
                text
            );
        }
        self.check_session()?;
        self.view.documents.preview_id = id.into();
        self.view.documents.preview_title = artifact["name"].as_str().unwrap_or("Artifact").into();
        self.view.documents.preview_text = text;
        self.view.documents.preview_path = path;
        Ok(())
    }
    pub fn download_review(&self) -> Result<DownloadReview> {
        if self.view.documents.preview_path.is_empty() {
            return Err("Open an artifact first.".into());
        }
        Ok(DownloadReview {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            name: self.view.documents.preview_title.clone(),
            path: self.view.documents.preview_path.clone(),
        })
    }
    pub fn download_document(&mut self, review: &DownloadReview, path: &Path) -> Result<()> {
        self.check_session()?;
        if self.session.as_deref() != Some(&review.session) {
            return Err("Export review belongs to an earlier workspace.".into());
        }
        let bytes = self.transport.download(&review.path, &review.session)?;
        self.check_session()?;
        write_new(path, &bytes)
    }
    pub fn answer_review(&self) -> Result<AnswerReview> {
        let conversation = self
            .view
            .conversation
            .as_ref()
            .ok_or("Open a conversation first.")?;
        let answer = conversation
            .active_messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && m.status != "streaming" && !m.content.is_empty())
            .ok_or("No completed visible answer to save.")?;
        Ok(AnswerReview {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            chat: conversation.chat.clone(),
            content: answer.content.clone(),
        })
    }
    pub fn save_answer(&mut self, review: &AnswerReview) -> Result<()> {
        self.check_session()?;
        if self.session.as_deref() != Some(&review.session)
            || self.view.conversation.as_ref().map(|c| &c.chat) != Some(&review.chat)
        {
            return Err("The displayed conversation changed. Review the answer again.".into());
        }
        self.transport.request("POST","/api/artifacts",self.session.as_deref(),Some(json!({"chat_id":review.chat.id,"content":review.content,"name":"Saved answer.txt"})))?;
        self.check_session()?;
        self.refresh_lists()?;
        self.refresh_conversation(&review.chat.id)
    }
    pub fn generate_table(&mut self, name: &str, format: &str, table: Value) -> Result<()> {
        self.check_session()?;
        let value = self.transport.request(
            "POST",
            "/api/media/generate",
            self.session.as_deref(),
            Some(json!({"request_id":random_id()?,"name":name,"format":format,"table":table})),
        )?;
        self.check_session()?;
        self.refresh_lists()?;
        if let Some(id) = value["artifact"]["id"]
            .as_str()
            .or_else(|| value["id"].as_str())
        {
            self.open_artifact(id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod import_download_tests {
    use super::*;
    #[test]
    fn only_exact_managed_original_identity_can_replace_projected_export() {
        let mut value = json!({"original_import":{"source_schema":25,"content_kind":"managed_original","byte_len":19,"sha256":"a".repeat(64),"download_url":"/api/legacy-import/artifacts/selected/bytes"}});
        assert_eq!(
            artifact_download_path(&value, "selected").unwrap(),
            "/api/legacy-import/artifacts/selected/bytes"
        );
        value["original_import"]["download_url"] = json!("https://example.invalid/selected");
        assert!(artifact_download_path(&value, "selected").is_err());
        value["original_import"]["download_url"] =
            json!("/api/legacy-import/artifacts/other/bytes");
        assert!(artifact_download_path(&value, "selected").is_err());
        value["original_import"]["download_url"] = Value::Null;
        assert_eq!(artifact_download_path(&value, "selected").unwrap(), "");
    }
    #[test]
    fn unavailable_and_oversize_imports_never_export_projected_text_as_original() {
        let value = json!({"original_import":{"source_schema":26,"content_kind":"managed_original","byte_len":MAX_FILE+1,"sha256":"a".repeat(64),"download_url":"/api/legacy-import/artifacts/selected/bytes"}});
        assert_eq!(artifact_download_path(&value, "selected").unwrap(), "");
        let value = json!({"original_import":{"source_schema":25,"content_kind":"external_reference","download_url":null}});
        assert_eq!(artifact_download_path(&value, "selected").unwrap(), "");
        assert_eq!(
            artifact_download_path(&json!({}), "selected").unwrap(),
            "/api/media/artifacts/selected/original"
        );
    }
}
