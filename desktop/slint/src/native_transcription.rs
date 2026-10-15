use super::*;
use crate::transcription::{
    SavedTranscript, TranscriptionJob, TranscriptionReview, TranscriptionSubmission,
    TranscriptionSubmitError,
};
use std::path::PathBuf;
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub review: Option<TranscriptionReview>,
    pub paths: Option<(String, String)>,
    pub job: Option<TranscriptionJob>,
    pub saved: Option<SavedTranscript>,
    pub uncertain: bool,
    pub save_uncertain: bool,
    pub save_retryable: bool,
    pub request_id: String,
    pub message: String,
    pub artifacts: Vec<serde_json::Value>,
}
#[derive(Default)]
pub(super) struct Worker {
    pub panel: Panel,
    submission: Option<TranscriptionSubmission>,
    save_attempt: Option<(TranscriptionJob, String, String, String)>,
    tick: u8,
}
pub(super) enum Action {
    Review(String, String),
    Start(TranscriptionReview, PathBuf, String, bool),
    Inspect,
    RetrySave,
    Cleanup(TranscriptionJob),
    Cancel(TranscriptionJob),
    Save(TranscriptionJob, String, String, String),
    Open(String),
    Export(TranscriptionJob, String, PathBuf),
    ExportSaved(SavedTranscript, String, PathBuf),
}
impl Worker {
    fn save<T: Transport>(&mut self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        let (job, id, title, format) = self
            .save_attempt
            .as_ref()
            .ok_or("No retained save request.")?;
        self.panel.save_uncertain = true;
        self.panel.save_retryable = false;
        b.save_transcription(job, id, title, format)?;
        self.panel.job = Some(b.poll_transcription(job)?);
        self.panel.save_uncertain = false;
        self.save_attempt = None;
        self.panel.message = "Transcript saved to Artifacts.".into();
        Ok(())
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn poll<T: Transport>(&mut self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        self.tick = (self.tick + 1) % 4;
        if self.tick != 0 {
            return Ok(());
        }
        if let Some(job) = self.panel.job.clone().filter(|v| v.running()) {
            self.panel.job = Some(b.poll_transcription(&job)?);
        }
        self.panel.artifacts = b.saved_transcription_artifacts();
        Ok(())
    }
    pub fn execute<T: Transport>(
        &mut self,
        b: &mut Bridge<T>,
        action: Action,
    ) -> crate::bridge::Result<()> {
        if (self.panel.uncertain
            || self.panel.save_uncertain
            || self
                .panel
                .job
                .as_ref()
                .is_some_and(|v| v.running() || v.snapshot["state"] == "cleanup_failed"))
            && matches!(
                &action,
                Action::Review(..) | Action::Start(..) | Action::Open(..) | Action::Save(..)
            )
        {
            return Err(
                "Inspect or finish the current transcription before changing its inputs.".into(),
            );
        }
        match action {
            Action::Review(exe, model) => {
                self.panel.review = Some(b.review_transcription(&exe, &model)?);
                self.panel.paths = Some((exe, model));
            }
            Action::Start(review, path, language, normalize) => {
                let submission = if normalize {
                    b.prepare_normalized_transcription(&review, &path, &language)?
                } else {
                    b.prepare_transcription(&review, &path, &language)?
                };
                self.panel.request_id = submission.id().into();
                self.submission = Some(submission);
                self.panel.uncertain = true;
                self.panel.saved = None;
                self.panel.job = None;
                let job = match b.submit_transcription(self.submission.as_ref().unwrap()) {
                    Ok(job) => job,
                    Err(TranscriptionSubmitError::NotSubmitted(error)) => {
                        self.panel.uncertain = false;
                        self.panel.request_id.clear();
                        self.submission = None;
                        return Err(error);
                    }
                    Err(TranscriptionSubmitError::OutcomeUnknown(error)) => return Err(error),
                };
                self.panel.job = Some(job);
                self.panel.uncertain = false;
                self.submission = None;
            }
            Action::Inspect if self.save_attempt.is_some() => {
                let job = &self.save_attempt.as_ref().unwrap().0;
                if b.recover_saved_transcription(job)?.is_some() {
                    self.panel.job = Some(b.poll_transcription(job)?);
                    self.panel.save_uncertain = false;
                    self.panel.save_retryable = false;
                    self.save_attempt = None;
                    self.panel.message = "Saved transcript confirmed in Artifacts.".into();
                } else {
                    self.panel.save_retryable = true;
                    self.panel.message="No saved receipt is available. An explicit retry uses the same save request.".into();
                }
            }
            Action::Inspect => {
                let submission = self
                    .submission
                    .as_ref()
                    .ok_or("No unconfirmed transcription is retained.")?;
                self.panel.job = Some(b.recover_transcription(submission)?);
                self.panel.uncertain = false;
                self.submission = None;
            }
            Action::Cancel(job) => self.panel.job = Some(b.cancel_transcription(&job)?),
            Action::Save(job, id, title, format) => {
                self.save_attempt = Some((job, id, title, format));
                self.save(b)?;
            }
            Action::RetrySave => {
                if !self.panel.save_retryable {
                    return Err("Inspect the saved transcript before retrying.".into());
                }
                self.save(b)?;
            }
            Action::Cleanup(job) => self.panel.job = Some(b.cleanup_transcription(&job)?),
            Action::Open(id) => {
                self.panel.saved = Some(b.open_saved_transcription(&id)?);
                self.panel.job = None;
            }
            Action::Export(job, format, path) => {
                b.export_transcription(&job, &format, &path)?;
                self.panel.message = "Transcript exported.".into();
            }
            Action::ExportSaved(saved, format, path) => {
                b.export_saved_transcription(&saved, &format, &path)?;
                self.panel.message = "Transcript exported.".into();
            }
        }
        self.panel.artifacts = b.saved_transcription_artifacts();
        Ok(())
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, a: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.transcription_pending {
        return;
    }
    s.transcription_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Transcription(a)));
}
fn format(w: &MainWindow) -> String {
    if w.get_transcribe_format() == "timestamps" {
        "timestamped".into()
    } else {
        w.get_transcribe_format().to_string()
    }
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    for kind in 0..3 {
        let state = state.clone();
        let weak = w.as_weak();
        let callback = move || {
            let Some(w) = weak.upgrade() else { return };
            let epoch = state.borrow().epoch.current();
            match crate::picker::file(match kind {
                0 => "Choose a trusted self-contained whisper.cpp executable",
                1 => "Choose an existing GGML speech model",
                _ => "Choose local WAV, AIFF or AAC/ALAC M4A audio (60 seconds maximum)",
            }) {
                Ok(Some(path)) if state.borrow().epoch.accepts(epoch) => {
                    let g = w.global::<NativeTranscription>();
                    match kind {
                        0 => {
                            g.set_executable(path.into());
                            g.set_confirmed(false);
                        }
                        1 => {
                            g.set_model(path.into());
                            g.set_confirmed(false);
                        }
                        _ => w.set_transcribe_source(path.into()),
                    }
                }
                Ok(_) => (),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        match kind {
            0 => w
                .global::<NativeTranscription>()
                .on_pick_executable(callback),
            1 => w.global::<NativeTranscription>().on_pick_model(callback),
            _ => w.on_transcribe_pick_file(callback),
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.global::<NativeTranscription>().on_review(move || {
            if let Some(w) = weak.upgrade() {
                let g = w.global::<NativeTranscription>();
                g.set_confirmed(false);
                enqueue(
                    &state,
                    &tx,
                    Action::Review(g.get_executable().to_string(), g.get_model().to_string()),
                );
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.on_transcribe_start(move || {
            let Some(w) = weak.upgrade() else { return };
            let g = w.global::<NativeTranscription>();
            let s = state.borrow();
            if !g.get_confirmed()
                || s.transcription_panel.paths.as_ref()
                    != Some(&(g.get_executable().to_string(), g.get_model().to_string()))
            {
                drop(s);
                state.borrow_mut().last_error =
                    Some("Review and confirm the selected local transcriber first.".into());
                return;
            }
            let review = s.transcription_panel.review.clone();
            drop(s);
            if let Some(review) = review {
                let language = match w.get_transcribe_lang().as_str() {
                    "English" => "en",
                    "Русский" => "ru",
                    _ => "auto",
                };
                enqueue(
                    &state,
                    &tx,
                    Action::Start(
                        review,
                        w.get_transcribe_source().as_str().into(),
                        language.into(),
                        g.get_normalize(),
                    ),
                );
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeTranscription>()
            .on_inspect(move || enqueue(&state, &tx, Action::Inspect));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeTranscription>()
            .on_retry_save(move || enqueue(&state, &tx, Action::RetrySave));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeTranscription>().on_cleanup(move || {
            let job = state.borrow().transcription_panel.job.clone();
            if let Some(job) = job {
                enqueue(&state, &tx, Action::Cleanup(job));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeTranscription>().on_cancel(move || {
            let job = state.borrow().transcription_panel.job.clone();
            if let Some(job) = job {
                enqueue(&state, &tx, Action::Cancel(job));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.on_transcribe_save_artifact(move || {
            let Some(w) = weak.upgrade() else { return };
            let job = state.borrow().transcription_panel.job.clone();
            if let Some(job) = job {
                match crate::documents::random_id() {
                    Ok(id) => {
                        let name = job.snapshot["source"]["name"]
                            .as_str()
                            .unwrap_or("Transcript")
                            .to_owned();
                        enqueue(&state, &tx, Action::Save(job, id, name, format(&w)));
                    }
                    Err(e) => state.borrow_mut().last_error = Some(e),
                }
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.on_transcribe_load_artifact(move |id| enqueue(&state, &tx, Action::Open(id.to_string())));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.on_transcribe_save(move || {
            let Some(w) = weak.upgrade() else { return };
            let (epoch, job, saved) = {
                let s = state.borrow();
                (
                    s.epoch.current(),
                    s.transcription_panel.job.clone(),
                    s.transcription_panel.saved.clone(),
                )
            };
            let format = format(&w);
            let filename = match format.as_str() {
                "srt" => "transcript.srt",
                "vtt" => "transcript.vtt",
                _ => "transcript.txt",
            };
            match crate::picker::save(filename) {
                Ok(Some(path)) if state.borrow().epoch.accepts(epoch) => {
                    if let Some(job) = job {
                        enqueue(&state, &tx, Action::Export(job, format, path.into()));
                    } else if let Some(saved) = saved {
                        enqueue(&state, &tx, Action::ExportSaved(saved, format, path.into()));
                    }
                }
                Ok(_) => (),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.on_transcribe_copy(move || {
            if let Some(w) = weak.upgrade() {
                if let Err(e) = crate::picker::copy_text(w.get_transcribe_output().as_str()) {
                    state.borrow_mut().last_error = Some(e);
                }
            }
        });
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let p = &s.transcription_panel;
    let g = w.global::<NativeTranscription>();
    let running = p.job.as_ref().is_some_and(TranscriptionJob::running);
    let cleanup_failed = p
        .job
        .as_ref()
        .is_some_and(|j| j.snapshot["state"] == "cleanup_failed");
    g.set_busy(s.transcription_pending);
    g.set_running(running);
    g.set_uncertain(p.uncertain || p.save_uncertain);
    g.set_save_uncertain(p.save_uncertain);
    g.set_save_retryable(p.save_retryable);
    g.set_cleanup_failed(cleanup_failed);
    let exact =
        p.paths.as_ref() == Some(&(g.get_executable().to_string(), g.get_model().to_string()));
    g.set_reviewed(p.review.is_some() && exact);
    let normalization_available = p
        .review
        .as_ref()
        .is_some_and(|r| r.summary["normalizer"]["adapter"] == "apple_afconvert");
    g.set_normalization_available(normalization_available);
    g.set_can_start(
        s.view.available
            && p.review.is_some()
            && exact
            && g.get_confirmed()
            && (!g.get_normalize() || normalization_available)
            && !running
            && !cleanup_failed
            && !p.uncertain
            && !p.save_uncertain
            && !s.transcription_pending,
    );
    if let Some(review) = &p.review {
        let r = &review.summary;
        g.set_summary(
            format!(
                "{} · {} · CPU transcription\nLanguages: {}",
                r["executable"]["name"]
                    .as_str()
                    .unwrap_or("Local executable"),
                r["model"]["name"].as_str().unwrap_or("Selected model"),
                r["allowed_languages"]
                    .as_array()
                    .map(|values| values
                        .iter()
                        .filter_map(|v| match v.as_str() {
                            Some("en") => Some("English"),
                            Some("ru") => Some("Russian"),
                            Some("auto") => Some("Auto-detect"),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(", "))
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "Review again to check language support".into())
            )
            .into(),
        );
        g.set_hashes(
            format!(
                "Executable SHA-256: {}\nModel SHA-256: {}\nAudio converter SHA-256: {}",
                r["executable"]["sha256"].as_str().unwrap_or(""),
                r["model"]["sha256"].as_str().unwrap_or(""),
                r["normalizer"]["executable"]["sha256"]
                    .as_str()
                    .unwrap_or("Unavailable")
            )
            .into(),
        );
    }
    let result = p
        .saved
        .as_ref()
        .map(|v| &v.payload["transcript"])
        .or_else(|| p.job.as_ref().map(|j| &j.snapshot["result"]));
    w.set_transcribe_out_text(result.and_then(|r| r["text"].as_str()).unwrap_or("").into());
    w.set_transcribe_out_timestamps(
        result
            .and_then(|r| r["timestamped"].as_str())
            .unwrap_or("")
            .into(),
    );
    w.set_transcribe_out_srt(result.and_then(|r| r["srt"].as_str()).unwrap_or("").into());
    w.set_transcribe_out_vtt(result.and_then(|r| r["vtt"].as_str()).unwrap_or("").into());
    w.set_transcribe_has_result(result.is_some_and(|r| r["text"].is_string()));
    w.set_transcribe_busy(s.transcription_pending || running || p.uncertain || p.save_uncertain);
    w.set_transcribe_status(
        if p.save_uncertain {
            format!(
                "Save is unconfirmed. Inspect the saved transcript before another save. {}",
                p.message
            )
        } else if p.uncertain {
            format!(
                "Transcription {} has an unconfirmed result. Inspect it without submitting again.",
                p.request_id
            )
        } else if let Some(j) = &p.job {
            format!(
                "{}{}{}",
                j.snapshot["state"].as_str().unwrap_or("Unknown"),
                j.snapshot["error"]["message"]
                    .as_str()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default(),
                if p.message.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", p.message)
                }
            )
        } else {
            p.message.clone()
        }
        .into(),
    );
    w.set_transcribe_artifacts(ModelRc::new(VecModel::from(
        p.artifacts
            .iter()
            .map(|a| TranscriptItem {
                id: a["id"].as_str().unwrap_or("").into(),
                title: a["name"].as_str().unwrap_or("Transcript").into(),
                subtitle: "Saved local transcript".into(),
            })
            .collect::<Vec<_>>(),
    )));
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.transcription_panel = Default::default();
    s.transcription_pending = false;
    let g = w.global::<NativeTranscription>();
    g.set_confirmed(false);
    g.set_normalize(false);
    g.set_normalization_available(false);
    g.set_executable("".into());
    g.set_model("".into());
    g.set_summary("".into());
    g.set_hashes("".into());
    w.set_transcribe_source("".into());
    project(w, s);
}
