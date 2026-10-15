//! Session-scoped native selected-file transcription. No microphone, discovery,
//! model download, or network provider is selected by these methods. Normalization
//! requires the explicitly reviewed fixed Apple system converter.
#[path = "../../../src/audio_normalization.rs"]
mod audio_normalization;
pub use crate::http::TranscriptionStartError as TranscriptionSubmitError;
use crate::{
    bridge::{encode_component, Bridge, Result, Transport},
    documents::{random_id, read_selected, write_new},
};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct TranscriptionReview {
    session: String,
    pub summary: Value,
}
#[derive(Clone)]
pub struct TranscriptionSubmission {
    review: TranscriptionReview,
    id: String,
    name: String,
    byte_len: usize,
    sha256: String,
    body: Value,
    normalized: Option<(String, f64, u16)>,
}
impl TranscriptionSubmission {
    pub fn id(&self) -> &str {
        &self.id
    }
}
impl std::fmt::Debug for TranscriptionSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscriptionSubmission")
            .field("request_id", &self.id)
            .field("byte_len", &self.byte_len)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug)]
pub struct SavedTranscript {
    session: String,
    pub artifact: Value,
    pub payload: Value,
}
#[derive(Clone, Debug)]
pub struct TranscriptionJob {
    session: String,
    pub snapshot: Value,
}
impl TranscriptionJob {
    pub fn id(&self) -> Result<&str> {
        self.snapshot["request_id"]
            .as_str()
            .ok_or_else(|| "Transcription identity is missing.".into())
    }
    pub fn running(&self) -> bool {
        matches!(
            self.snapshot["state"].as_str(),
            Some("running" | "cancelling")
        )
    }
}
fn valid_id(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 36
            && s.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
                }
            })
    })
}
fn audio_sha256(bytes: &[u8]) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn CC_SHA256(data: *const libc::c_void, len: u32, digest: *mut u8) -> *mut u8;
        }
        let length = u32::try_from(bytes.len()).map_err(|_| "Audio exceeds the hash bound.")?;
        let mut digest = [0u8; 32];
        // macOS CommonCrypto is part of libSystem. The bounded immutable input
        // and full 32-byte output remain alive throughout this synchronous call.
        let result = unsafe { CC_SHA256(bytes.as_ptr().cast(), length, digest.as_mut_ptr()) };
        if result != digest.as_mut_ptr() {
            return Err("The system audio digest operation failed.".into());
        }
        Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bytes;
        Err("Selected local transcription is currently supported only on macOS.".into())
    }
}
fn validate_wav(bytes: &[u8]) -> Result<()> {
    let reject = || {
        "Select PCM16 mono 16 kHz WAV with only format/data chunks, at most 60 seconds and 2 MiB."
            .to_owned()
    };
    if bytes.len() < 44
        || bytes.len() > 2 * 1024 * 1024
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
        || u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8 != bytes.len()
    {
        return Err(reject());
    }
    let mut at = 12;
    let mut format = false;
    let mut data = None;
    while at < bytes.len() {
        let header = bytes.get(at..at + 8).ok_or_else(reject)?;
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        at += 8;
        let chunk = bytes.get(at..at + len).ok_or_else(reject)?;
        match &header[..4] {
            b"fmt " => {
                if format || len != 16 {
                    return Err(reject());
                }
                let n = |i| u16::from_le_bytes(chunk[i..i + 2].try_into().unwrap());
                if n(0) != 1
                    || n(2) != 1
                    || u32::from_le_bytes(chunk[4..8].try_into().unwrap()) != 16000
                    || u32::from_le_bytes(chunk[8..12].try_into().unwrap()) != 32000
                    || n(12) != 2
                    || n(14) != 16
                {
                    return Err(reject());
                }
                format = true;
            }
            b"data" => {
                if data.replace(len).is_some() {
                    return Err(reject());
                }
            }
            _ => return Err(reject()),
        };
        at = at.checked_add(len + len % 2).ok_or_else(reject)?;
    }
    if !format || at != bytes.len() || data.is_none_or(|n| n == 0 || n % 2 != 0 || n > 60 * 32000) {
        return Err(reject());
    }
    Ok(())
}
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}
impl<T: Transport> Bridge<T> {
    pub fn review_transcription(
        &mut self,
        executable_path: &str,
        model_path: &str,
    ) -> Result<TranscriptionReview> {
        self.check_session()?;
        let session = self.session.clone().ok_or("Workspace unavailable.")?;
        let summary = self.transport.request(
            "POST",
            "/api/media/transcription/review",
            Some(&session),
            Some(json!({"executable_path":executable_path,"model_path":model_path})),
        )?;
        self.check_session()?;
        if self.session.as_ref() != Some(&session)
            || !valid_id(&summary["review_id"])
            || summary["adapter"] != "whisper_cpp_cli"
            || summary["verified_model"] != false
            || summary["notice"].as_str().is_none()
            || !matches!(summary["allowed_languages"].as_array(), Some(languages) if languages == &vec![json!("en")] || languages == &vec![json!("en"),json!("ru"),json!("auto")])
        {
            return Err("Malformed or stale local transcriber review.".into());
        }
        Ok(TranscriptionReview { session, summary })
    }
    /// Retain this submission before sending. A lost POST reply is recovered by
    /// its exact ID, without another transcription or a new request identity.
    pub fn prepare_transcription(
        &mut self,
        review: &TranscriptionReview,
        path: &Path,
        language: &str,
    ) -> Result<TranscriptionSubmission> {
        self.prepare_transcription_input(review, path, language, false)
    }
    /// Explicit selected-file normalization through the reviewed Apple converter.
    /// The opaque submission retains the exact original bytes and recovery ID.
    pub fn prepare_normalized_transcription(
        &mut self,
        review: &TranscriptionReview,
        path: &Path,
        language: &str,
    ) -> Result<TranscriptionSubmission> {
        self.prepare_transcription_input(review, path, language, true)
    }
    fn prepare_transcription_input(
        &mut self,
        review: &TranscriptionReview,
        path: &Path,
        language: &str,
        normalize: bool,
    ) -> Result<TranscriptionSubmission> {
        self.check_session()?;
        if self.session.as_ref() != Some(&review.session)
            || !matches!(language, "en" | "ru" | "auto")
        {
            return Err("Review the local transcriber again in this workspace.".into());
        }
        if !review.summary["allowed_languages"]
            .as_array()
            .is_some_and(|languages| {
                languages
                    .iter()
                    .any(|value| value.as_str() == Some(language))
            })
        {
            return Err("The selected model does not declare support for this language. Select an allowed language or review another model.".into());
        }
        let (name, bytes) = read_selected(path)?;
        let normalized = if normalize {
            if review.summary["normalizer"]["adapter"] != "apple_afconvert"
                || review.summary["normalizer"]["executable"]["sha256"]
                    .as_str()
                    .is_none_or(|s| s.len() != 64)
            {
                return Err("Review the available Apple system converter before normalizing selected audio.".into());
            }
            let audio=audio_normalization::inspect(&name,&bytes).map_err(|_|"Select supported self-contained PCM WAV/AIFF or AAC-LC/ALAC M4A, at most 8 MiB and 60 seconds. Video, external references and unsupported metadata are refused.".to_owned())?;
            Some((
                audio.format.name().to_owned(),
                audio.seconds,
                audio.channels,
            ))
        } else {
            if bytes.len() > 2 * 1024 * 1024 || !name.to_ascii_lowercase().ends_with(".wav") {
                return Err("Select PCM16 mono 16 kHz WAV, at most 2 MiB and 60 seconds, or explicitly enable supported file normalization.".into());
            }
            validate_wav(&bytes)?;
            None
        };
        let id = random_id()?;
        Ok(TranscriptionSubmission {
            review: review.clone(),
            id: id.clone(),
            name: name.clone(),
            byte_len: bytes.len(),
            sha256: audio_sha256(&bytes)?,
            body: if normalize {
                json!({"review_id":review.summary["review_id"],"request_id":id,"name":name,"audio_base64":base64(&bytes),"language":language})
            } else {
                json!({"review_id":review.summary["review_id"],"request_id":id,"name":name,"wav_base64":base64(&bytes),"language":language})
            },
            normalized,
        })
    }
    pub fn submit_transcription(
        &mut self,
        submission: &TranscriptionSubmission,
    ) -> std::result::Result<TranscriptionJob, TranscriptionSubmitError> {
        self.submission_current(submission)
            .map_err(TranscriptionSubmitError::NotSubmitted)?;
        let snapshot = if submission.normalized.is_some() {
            self.transport
                .transcription_file_start(&submission.review.session, submission.body.clone())?
        } else {
            self.transport
                .transcription_start(&submission.review.session, submission.body.clone())?
        };
        self.accept_transcription(submission, snapshot)
            .map_err(TranscriptionSubmitError::OutcomeUnknown)
    }
    pub fn recover_transcription(
        &mut self,
        submission: &TranscriptionSubmission,
    ) -> Result<TranscriptionJob> {
        self.submission_current(submission)?;
        let snapshot = self.get(&format!(
            "/api/media/transcription/jobs/{}",
            encode_component(&submission.id)
        ))?;
        self.accept_transcription(submission, snapshot)
    }
    fn submission_current(&mut self, submission: &TranscriptionSubmission) -> Result<()> {
        self.check_session()?;
        if self.session.as_ref() != Some(&submission.review.session) {
            return Err("Transcription submission belongs to a previous workspace.".into());
        }
        Ok(())
    }
    fn accept_transcription(
        &mut self,
        submission: &TranscriptionSubmission,
        snapshot: Value,
    ) -> Result<TranscriptionJob> {
        self.submission_current(submission)?;
        if let Some((format, seconds, channels)) = &submission.normalized {
            if snapshot["source"]["normalization_requested"] != true
                || snapshot["source"]["format"].as_str() != Some(format.as_str())
                || snapshot["source"]["audio_seconds"].as_f64() != Some(*seconds)
                || snapshot["source"]["channels"].as_u64() != Some(u64::from(*channels))
                || snapshot["provenance"]["normalizer"]
                    != submission.review.summary["normalizer"]["executable"]
            {
                return Err("Normalized transcription does not match the exact reviewed source and system converter.".into());
            }
        }
        if snapshot["request_id"] != submission.id
            || snapshot["source"]["name"] != submission.name
            || snapshot["source"]["byte_len"] != submission.byte_len
            || snapshot["source"]["sha256"] != submission.sha256
            || snapshot["provenance"]["executable"] != submission.review.summary["executable"]
            || snapshot["provenance"]["model"] != submission.review.summary["model"]
        {
            return Err(
                "Transcription admission did not match the selected file and reviewed assets."
                    .into(),
            );
        }
        Ok(TranscriptionJob {
            session: submission.review.session.clone(),
            snapshot,
        })
    }
    pub fn poll_transcription(&mut self, job: &TranscriptionJob) -> Result<TranscriptionJob> {
        self.transcription_current(job)?;
        let snapshot = self.get(&format!(
            "/api/media/transcription/jobs/{}",
            encode_component(job.id()?)
        ))?;
        self.transcription_current(job)?;
        if snapshot["request_id"] != job.snapshot["request_id"]
            || snapshot["source"] != job.snapshot["source"]
            || snapshot["provenance"] != job.snapshot["provenance"]
        {
            return Err("Transcription response identity changed.".into());
        }
        Ok(TranscriptionJob {
            session: job.session.clone(),
            snapshot,
        })
    }
    pub fn cancel_transcription(&mut self, job: &TranscriptionJob) -> Result<TranscriptionJob> {
        self.transcription_current(job)?;
        self.transport.request(
            "POST",
            &format!(
                "/api/media/transcription/jobs/{}/cancel",
                encode_component(job.id()?)
            ),
            Some(&job.session),
            Some(json!({})),
        )?;
        self.poll_transcription(job)
    }
    pub fn cleanup_transcription(&mut self, job: &TranscriptionJob) -> Result<TranscriptionJob> {
        self.transcription_current(job)?;
        if job.snapshot["state"] != "cleanup_failed" {
            return Err("This job does not report retained cleanup ownership.".into());
        }
        let response = self.transport.request(
            "POST",
            "/api/media/transcription/cleanup",
            Some(&job.session),
            Some(json!({})),
        )?;
        self.transcription_current(job)?;
        if response["cleanup_confirmed"] != true {
            return Err(
                "Transcription cleanup was not confirmed; retain this job and retry inspection."
                    .into(),
            );
        }
        self.poll_transcription(job)
    }
    /// Recover a committed save with GET only. None means no receipt was
    /// observed; a later explicit retry must keep the original UUID and body.
    pub fn recover_saved_transcription(&mut self, job: &TranscriptionJob) -> Result<Option<Value>> {
        let current = self.poll_transcription(job)?;
        let saved = &current.snapshot["saved"];
        if saved.is_null() {
            return Ok(None);
        }
        self.validate_transcription_save(&current, saved)?;
        self.refresh_transcription_artifacts(&current)?;
        Ok(Some(saved.clone()))
    }
    fn validate_transcription_save(&self, job: &TranscriptionJob, response: &Value) -> Result<()> {
        if !valid_id(&response["artifact"]["id"])
            || response["document"]["metadata"]["operation"] != "local_transcription"
            || response["document"]["metadata"]["transcription_job"] != job.snapshot["request_id"]
            || response["document"]["metadata"]["source_sha256"] != job.snapshot["source"]["sha256"]
        {
            return Err("The saved transcript receipt does not match this job and source.".into());
        }
        Ok(())
    }
    fn refresh_transcription_artifacts(&mut self, job: &TranscriptionJob) -> Result<()> {
        let response = self.get("/api/media/artifacts")?;
        self.transcription_current(job)?;
        self.view.artifacts = response
            .as_array()
            .or_else(|| response["artifacts"].as_array())
            .ok_or("Malformed transcript artifact list.")?
            .clone();
        Ok(())
    }
    pub fn save_transcription(
        &mut self,
        job: &TranscriptionJob,
        request_id: &str,
        title: &str,
        format: &str,
    ) -> Result<Value> {
        self.transcription_current(job)?;
        if job.snapshot["state"] != "completed"
            || !matches!(format, "text" | "timestamped" | "srt" | "vtt")
        {
            return Err("Wait for a completed transcript before saving.".into());
        }
        let response = self.transport.request(
            "POST",
            &format!(
                "/api/media/transcription/jobs/{}/save",
                encode_component(job.id()?)
            ),
            Some(&job.session),
            Some(json!({"request_id":request_id,"title":title,"format":format})),
        )?;
        self.transcription_current(job)?;
        self.validate_transcription_save(job, &response)?;
        self.refresh_transcription_artifacts(job)?;
        Ok(response)
    }
    pub fn export_transcription(
        &mut self,
        job: &TranscriptionJob,
        format: &str,
        path: &Path,
    ) -> Result<()> {
        let current = self.poll_transcription(job)?;
        if current.snapshot["state"] != "completed"
            || !matches!(format, "text" | "timestamped" | "srt" | "vtt")
        {
            return Err("Only a completed current transcript can be exported.".into());
        }
        let text = current.snapshot["result"][format]
            .as_str()
            .ok_or("Transcript format is unavailable.")?;
        write_new(path, text.as_bytes())
    }
    /// Only currently listed artifacts marked by the backend as transcription
    /// outputs. Opening still validates the exact downloaded canonical JSON.
    pub fn saved_transcription_artifacts(&self) -> Vec<Value> {
        self.view
            .artifacts
            .iter()
            .filter(|v| v["document"]["metadata"]["operation"] == "local_transcription")
            .cloned()
            .collect()
    }
    pub fn open_saved_transcription(&mut self, id: &str) -> Result<SavedTranscript> {
        self.check_session()?;
        let session = self.session.clone().ok_or("Workspace unavailable.")?;
        let artifact = self
            .saved_transcription_artifacts()
            .into_iter()
            .find(|v| v["id"] == id)
            .ok_or("Refresh saved transcript artifacts first.")?;
        let bytes = self.transport.download(
            &format!("/api/media/artifacts/{}/original", encode_component(id)),
            &session,
        )?;
        self.check_session()?;
        if self.session.as_ref() != Some(&session) || bytes.len() > 1024 * 1024 {
            return Err("Saved transcript is stale or exceeds its size limit.".into());
        }
        let payload: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Saved transcript JSON is malformed.")?;
        if payload["schema"] != "smith.transcript.v1"
            || payload["source"]["sha256"] != artifact["document"]["metadata"]["source_sha256"]
            || payload["provenance"]["adapter"] != "whisper_cpp_cli"
            || !matches!(
                payload["format"].as_str(),
                Some("text" | "timestamped" | "srt" | "vtt")
            )
            || payload["transcript"]["segments"]
                .as_array()
                .is_none_or(|s| s.len() > 1000)
            || ["text", "timestamped", "srt", "vtt"].iter().any(|key| {
                payload["transcript"][key]
                    .as_str()
                    .is_none_or(|s| s.len() > 256 * 1024)
            })
        {
            return Err("This artifact is not a supported saved local transcript.".into());
        }
        Ok(SavedTranscript {
            session,
            artifact,
            payload,
        })
    }
    pub fn export_saved_transcription(
        &mut self,
        saved: &SavedTranscript,
        format: &str,
        path: &Path,
    ) -> Result<()> {
        self.check_session()?;
        if self.session.as_ref() != Some(&saved.session)
            || !matches!(format, "text" | "timestamped" | "srt" | "vtt")
        {
            return Err(
                "Saved transcript export belongs to a previous workspace or unsupported format."
                    .into(),
            );
        }
        let text = saved.payload["transcript"][format]
            .as_str()
            .ok_or("Saved transcript format unavailable.")?;
        write_new(path, text.as_bytes())
    }
    fn transcription_current(&mut self, job: &TranscriptionJob) -> Result<()> {
        self.check_session()?;
        if self.session.as_ref() != Some(&job.session) || !valid_id(&job.snapshot["request_id"]) {
            return Err("Transcription belongs to a previous workspace session.".into());
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_preserves_binary_and_padding() {
        for (bytes, encoded) in [
            (b"".as_slice(), ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (&[0, 255, 128], "AP+A"),
        ] {
            assert_eq!(base64(bytes), encoded);
        }
    }
    #[test]
    fn identities_are_bounded_before_paths() {
        assert!(valid_id(&json!("00000000-0000-4000-8000-000000000001")));
        for value in [
            json!("../jobs"),
            json!("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA"),
            Value::Null,
        ] {
            assert!(!valid_id(&value));
        }
    }
    struct LostReply {
        posts: usize,
        gets: usize,
        snapshot: Value,
        session: String,
        bytes: Vec<u8>,
        source_sha256: String,
        saves: usize,
        cleanups: usize,
    }
    impl Transport for LostReply {
        fn request(
            &mut self,
            method: &str,
            path: &str,
            _session: Option<&str>,
            body: Option<Value>,
        ) -> Result<Value> {
            if path == "/api/profile/status" {
                return Ok(json!({"state":"unlocked","session":self.session}));
            }
            if path == "/api/media/transcription/cleanup" && method == "POST" {
                self.cleanups += 1;
                self.snapshot["state"] = json!("failed");
                return Ok(json!({"cleanup_confirmed":true}));
            }
            if path.ends_with("/save") && method == "POST" {
                self.saves += 1;
                self.snapshot["saved"] = json!({"artifact":{"id":"00000000-0000-4000-8000-000000000002"},"document":{"metadata":{"operation":"local_transcription","transcription_job":self.snapshot["request_id"],"source_sha256":self.snapshot["source"]["sha256"]}}});
                return Err("Simulated lost save reply after commit".into());
            }
            if path == "/api/media/artifacts" && method == "GET" {
                return Ok(json!({"artifacts":[self.snapshot["saved"]["artifact"]]}));
            }
            if path == "/api/media/transcription/start-file" && method == "POST" {
                self.posts += 1;
                let b = body.unwrap();
                assert!(b.get("wav_base64").is_none());
                assert!(b["audio_base64"].as_str().is_some());
                let audio =
                    audio_normalization::inspect(b["name"].as_str().unwrap(), &self.bytes).unwrap();
                self.snapshot = json!({"request_id":b["request_id"],"state":"running","source":{"name":b["name"],"byte_len":self.bytes.len(),"sha256":self.source_sha256,"format":audio.format.name(),"channels":audio.channels,"audio_seconds":audio.seconds,"normalization_requested":true},"provenance":{"executable":{"sha256":"exe"},"model":{"sha256":"model"},"normalizer":{"sha256":"a".repeat(64)}},"result":null});
                return Err("Simulated lost normalized-file admission reply".into());
            }
            if path == "/api/media/transcription/start" && method == "POST" {
                self.posts += 1;
                let b = body.unwrap();
                self.snapshot = json!({"request_id":b["request_id"],"state":"completed","source":{"name":b["name"],"byte_len":46,"sha256":self.source_sha256},"provenance":{"executable":{"sha256":"exe"},"model":{"sha256":"model"}},"result":{"text":"fixture"}});
                return Err("Simulated lost reply after admission".into());
            }
            if path.starts_with("/api/media/transcription/jobs/") && method == "GET" {
                self.gets += 1;
                return Ok(self.snapshot.clone());
            }
            Err("Unexpected fixture request".into())
        }
        fn download(&mut self, _path: &str, _session: &str) -> Result<Vec<u8>> {
            Ok(self.bytes.clone())
        }
    }
    fn fixture_bridge() -> Bridge<LostReply> {
        let mut b = Bridge::new(LostReply {
            posts: 0,
            gets: 0,
            snapshot: Value::Null,
            session: "session".into(),
            bytes: vec![],
            source_sha256: String::new(),
            saves: 0,
            cleanups: 0,
        });
        b.session = Some("session".into());
        b
    }
    #[test]
    fn system_audio_digest_matches_sha256_known_vector() {
        assert_eq!(
            audio_sha256(b"abc").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn lost_save_reply_recovers_by_get_and_cleanup_keeps_exact_job_session() {
        let mut bridge = fixture_bridge();
        let snapshot = json!({"request_id":"00000000-0000-4000-8000-000000000001","state":"completed","source":{"sha256":"a".repeat(64)},"provenance":{"model":"fixture"},"saved":null});
        bridge.transport.snapshot = snapshot.clone();
        let job = TranscriptionJob {
            session: "session".into(),
            snapshot,
        };
        assert!(bridge.recover_saved_transcription(&job).unwrap().is_none());
        assert!(bridge
            .save_transcription(
                &job,
                "00000000-0000-4000-8000-000000000003",
                "Fixture",
                "text"
            )
            .is_err());
        let saved = bridge.recover_saved_transcription(&job).unwrap().unwrap();
        assert_eq!(bridge.transport.saves, 1);
        assert_eq!(bridge.view.artifacts[0]["id"], saved["artifact"]["id"]);
        bridge.transport.snapshot["saved"]["document"]["metadata"]["source_sha256"] =
            json!("b".repeat(64));
        assert!(bridge.recover_saved_transcription(&job).is_err());
        bridge.transport.snapshot["state"] = json!("cleanup_failed");
        let retained = TranscriptionJob {
            session: "session".into(),
            snapshot: bridge.transport.snapshot.clone(),
        };
        assert_eq!(
            bridge.cleanup_transcription(&retained).unwrap().snapshot["state"],
            "failed"
        );
        assert_eq!(bridge.transport.cleanups, 1);
        bridge.transport.session = "replacement".into();
        assert!(bridge.cleanup_transcription(&retained).is_err());
        assert_eq!(bridge.transport.cleanups, 1);
    }
    #[test]
    fn unknown_admission_retains_exact_id_and_recovers_without_second_post() {
        let mut bridge = fixture_bridge();
        let review = TranscriptionReview {
            session: "session".into(),
            summary: json!({"review_id":"00000000-0000-4000-8000-000000000001","allowed_languages":["en"],"executable":{"sha256":"exe"},"model":{"sha256":"model"}}),
        };
        let path = std::env::temp_dir().join(format!("smith-audio-{}.wav", random_id().unwrap()));
        let mut wave = b"RIFF".to_vec();
        wave.extend(38u32.to_le_bytes());
        wave.extend(b"WAVEfmt ");
        wave.extend(16u32.to_le_bytes());
        wave.extend(1u16.to_le_bytes());
        wave.extend(1u16.to_le_bytes());
        wave.extend(16000u32.to_le_bytes());
        wave.extend(32000u32.to_le_bytes());
        wave.extend(2u16.to_le_bytes());
        wave.extend(16u16.to_le_bytes());
        wave.extend(b"data");
        wave.extend(2u32.to_le_bytes());
        wave.extend([0u8, 0]);
        bridge.transport.source_sha256 = audio_sha256(&wave).unwrap();
        std::fs::write(&path, wave).unwrap();
        assert!(bridge.prepare_transcription(&review, &path, "ru").is_err());
        assert!(bridge
            .prepare_transcription(&review, &path, "auto")
            .is_err());
        assert_eq!(bridge.transport.posts, 0);
        let prepared = bridge.prepare_transcription(&review, &path, "en").unwrap();
        std::fs::remove_file(path).unwrap();
        let id = prepared.id().to_owned();
        assert!(bridge.submit_transcription(&prepared).is_err());
        let recovered = bridge.recover_transcription(&prepared).unwrap();
        assert_eq!(recovered.id().unwrap(), id);
        assert_eq!(bridge.transport.posts, 1);
        assert_eq!(bridge.transport.gets, 1);
        bridge.transport.snapshot["source"]["sha256"] = json!("0".repeat(64));
        assert!(bridge.recover_transcription(&prepared).is_err());
        bridge.transport.session = "replacement".into();
        assert!(bridge.recover_transcription(&prepared).is_err());
        assert_eq!(bridge.transport.posts, 1);
        assert!(!bridge.view.available);
    }
    #[test]
    fn saved_transcript_reopens_separately_from_runtime_and_exports_exact_selected_format() {
        let mut bridge = fixture_bridge();
        let id = "00000000-0000-4000-8000-000000000001";
        let artifact = json!({"id":id,"document":{"metadata":{"operation":"local_transcription","source_sha256":"source"}}});
        bridge.view.artifacts = vec![artifact];
        let payload = json!({"schema":"smith.transcript.v1","source":{"sha256":"source"},"provenance":{"adapter":"whisper_cpp_cli"},"format":"vtt","transcript":{"segments":[],"text":"Привет","timestamped":"[00:00:00] Привет","srt":"fixture srt","vtt":"WEBVTT\n\n"}});
        bridge.transport.bytes = serde_json::to_vec(&payload).unwrap();
        let saved = bridge.open_saved_transcription(id).unwrap();
        assert_eq!(saved.payload, payload);
        let path =
            std::env::temp_dir().join(format!("smith-transcript-{}.vtt", random_id().unwrap()));
        bridge
            .export_saved_transcription(&saved, "vtt", &path)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"WEBVTT\n\n");
        assert!(bridge
            .export_saved_transcription(&saved, "text", &path)
            .is_err());
        std::fs::remove_file(path).unwrap();
        bridge.transport.bytes = b"{}".to_vec();
        assert!(bridge.open_saved_transcription(id).is_err());
    }
    #[test]
    fn normalized_file_submission_retains_original_digest_and_exact_decoder_on_lost_reply() {
        let mut bridge = fixture_bridge();
        let bytes = include_bytes!("../../../tests/fixtures/normalization-aac.m4a").to_vec();
        bridge.transport.bytes = bytes.clone();
        bridge.transport.source_sha256 = audio_sha256(&bytes).unwrap();
        let review = TranscriptionReview {
            session: "session".into(),
            summary: json!({"review_id":"00000000-0000-4000-8000-000000000001","allowed_languages":["en"],"executable":{"sha256":"exe"},"model":{"sha256":"model"},"normalizer":{"adapter":"apple_afconvert","executable":{"sha256":"a".repeat(64)}}}),
        };
        let path =
            std::env::temp_dir().join(format!("smith-normalize-{}.m4a", random_id().unwrap()));
        std::fs::write(&path, bytes).unwrap();
        assert!(bridge.prepare_transcription(&review, &path, "en").is_err());
        let submitted = bridge
            .prepare_normalized_transcription(&review, &path, "en")
            .unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            bridge.submit_transcription(&submitted),
            Err(TranscriptionSubmitError::OutcomeUnknown(_))
        ));
        assert_eq!(
            bridge
                .recover_transcription(&submitted)
                .unwrap()
                .id()
                .unwrap(),
            submitted.id()
        );
        assert_eq!(bridge.transport.posts, 1);
        bridge.transport.snapshot["provenance"]["normalizer"]["sha256"] = json!("b".repeat(64));
        assert!(bridge.recover_transcription(&submitted).is_err());
        assert_eq!(bridge.transport.posts, 1);
    }
}
