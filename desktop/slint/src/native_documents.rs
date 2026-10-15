use super::*;
use crate::conversation_export::ConversationExportReview;
use crate::documents::{AnswerReview, DocumentReview, DownloadReview};
use serde_json::{json, Value};
use std::path::PathBuf;
#[derive(Clone, Default)]
pub(super) struct Reviews {
    pub attach: Option<DocumentReview>,
    pub download: Option<DownloadReview>,
    pub answer: Option<AnswerReview>,
    pub conversations: std::collections::BTreeMap<String, ConversationExportReview>,
}
impl Reviews {
    pub fn capture<T: Transport>(bridge: &Bridge<T>) -> Self {
        Self {
            attach: bridge.document_review().ok(),
            download: bridge.download_review().ok(),
            answer: bridge.answer_review().ok(),
            conversations: bridge.conversation_export_reviews(),
        }
    }
}
pub(super) enum Action {
    Import(u64, PathBuf),
    Attach(DocumentReview, String),
    Promote(DocumentReview),
    Clear(u64),
    Open(String),
    Select(u64, String),
    Export(DownloadReview, PathBuf),
    ExportConversation(ConversationExportReview, PathBuf),
    Save(AnswerReview),
    Generate(String, String, Value),
}
impl Action {
    pub(super) fn execute<T: Transport>(self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        match self {
            Self::Import(i, p) => b.inspect_document(i, &p),
            Self::Attach(r, d) => b.attach_document(&r, &d),
            Self::Promote(r) => b.promote_document(&r),
            Self::Clear(i) => b.clear_documents(i),
            Self::Open(id) => b.open_artifact(&id),
            Self::Select(i, id) => b.review_saved_document(i, &id),
            Self::Export(r, p) => b.download_document(&r, &p),
            Self::ExportConversation(r, p) => b.export_conversation(&r, &p),
            Self::Save(r) => b.save_answer(&r),
            Self::Generate(n, f, t) => b.generate_table(&n, &f, t),
        }
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, action: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.documents_pending {
        return;
    }
    s.documents_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Documents(action)));
}
fn fail(state: &Rc<RefCell<UiState>>, error: impl Into<String>) {
    state.borrow_mut().last_error = Some(error.into());
}
pub(super) fn wire_export(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
    choose: impl Fn() -> crate::bridge::Result<Option<String>> + 'static,
) {
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.on_export_session(move |index| {
            let Some(w) = weak.upgrade() else { return };
            let (epoch, review) = {
                let mut s = state.borrow_mut();
                if !s.view.available
                    || s.documents_pending
                    || s.trash
                    || w.get_export_session_saving()
                    || w.get_session_loading()
                    || !w.get_chat_operation().is_empty()
                {
                    return;
                }
                let review = usize::try_from(index)
                    .ok()
                    .and_then(|i| filtered_chats(&s).get(i).copied())
                    .and_then(|row| {
                        // Branch and generation replies carry a newer selected
                        // chat version than the sidebar cache. Capture that
                        // already displayed owner, never refresh a queued review.
                        let row = s
                            .view
                            .conversation
                            .as_ref()
                            .filter(|c| c.chat.id == row.id)
                            .map(|c| &c.chat)
                            .unwrap_or(row);
                        s.document_reviews
                            .conversations
                            .get(&row.id)
                            .filter(|review| review.matches(row))
                    })
                    .cloned();
                let Some(review) = review else {
                    s.last_error = Some(
                        "Wait for the conversation to finish saving or reload it before exporting."
                            .into(),
                    );
                    return;
                };
                s.export_notice = None;
                (s.epoch.current(), review)
            };
            w.set_export_session_saving(true);
            let result = choose();
            w.set_export_session_saving(false);
            if !state.borrow().epoch.accepts(epoch) {
                return;
            }
            match result {
                Ok(Some(path)) => {
                    let mut s = state.borrow_mut();
                    if !s.view.available || s.documents_pending {
                        return;
                    }
                    s.documents_pending = true;
                    s.export_pending = true;
                    w.set_export_session_saving(true);
                    if tx
                        .send((
                            epoch,
                            Command::Documents(Action::ExportConversation(review, path.into())),
                        ))
                        .is_err()
                    {
                        s.documents_pending = false;
                        s.export_pending = false;
                        w.set_export_session_saving(false);
                        s.last_error = Some(
                            "The native worker stopped before exporting. No export was confirmed."
                                .into(),
                        );
                    }
                }
                Ok(None) => (),
                Err(error) => fail(&state, error),
            }
        });
    }
}
pub(super) fn wire(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    wire_export(window, state, tx, crate::picker::save_conversation);
    for register in [
        MainWindow::on_attach_file,
        MainWindow::on_artifacts_register_folder,
    ] {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        register(window, move || {
            let Some(w) = weak.upgrade() else { return };
            let (epoch, identity) = {
                let mut s = state.borrow_mut();
                if !s.view.available || s.documents_pending || s.editing.is_some() {
                    return;
                }
                capture_draft(&w, &tx, &mut s);
                (s.epoch.current(), s.view.draft.identity)
            };
            let result=crate::picker::file("Choose a document to inspect. It is not sent to a model until you attach it and send a message.");
            if !state.borrow().epoch.accepts(epoch) {
                return;
            }
            match result {
                Ok(Some(path)) => enqueue(&state, &tx, Action::Import(identity, path.into())),
                Ok(None) => (),
                Err(e) => fail(&state, e),
            }
        });
    }
    for register in [
        MainWindow::on_clear_attachment,
        MainWindow::on_clear_pending_artifact,
    ] {
        let state = state.clone();
        let tx = tx.clone();
        register(window, move || {
            let i = state.borrow().view.draft.identity;
            enqueue(&state, &tx, Action::Clear(i));
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_artifacts_open(move |id| enqueue(&state, &tx, Action::Open(id.into())));
    }
    {
        let weak = window.as_weak();
        window.on_open_artifact_picker(move || {
            if let Some(w) = weak.upgrade() {
                w.set_artifact_picker_open(true);
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.on_pick_artifact(move|id|{
        let (identity,media)={let s=state.borrow();(s.view.draft.identity,s.view.artifacts.iter().find(|v|v["id"]==id.as_str()).and_then(|v|v["document"]["id"].as_str()).map(str::to_owned))};
        if let Some(w)=weak.upgrade(){w.set_artifact_picker_open(false);}
        if let Some(id)=media{enqueue(&state,&tx,Action::Select(identity,id));}else{fail(&state,"This saved answer is available for preview/export. Attach a supported document file to provide model context.");}
    });
    }
    for reference in [false, true] {
        let state = state.clone();
        let tx = tx.clone();
        let handler = move || {
            let r = state.borrow().document_reviews.attach.clone();
            if let Some(r) = r {
                enqueue(
                    &state,
                    &tx,
                    Action::Attach(
                        r,
                        if reference {
                            "reference_only"
                        } else {
                            "context"
                        }
                        .into(),
                    ),
                );
            }
        };
        if reference {
            window
                .global::<NativeDocuments>()
                .on_attach_reference(handler);
        } else {
            window
                .global::<NativeDocuments>()
                .on_attach_context(handler);
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.global::<NativeDocuments>().on_promote(move || {
            let r = state.borrow().document_reviews.attach.clone();
            if let Some(r) = r {
                enqueue(&state, &tx, Action::Promote(r));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.global::<NativeDocuments>().on_clear(move || {
            let i = state.borrow().view.draft.identity;
            enqueue(&state, &tx, Action::Clear(i));
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.global::<NativeDocuments>().on_save_answer(move || {
            let r = state.borrow().document_reviews.answer.clone();
            if let Some(r) = r {
                enqueue(&state, &tx, Action::Save(r));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.global::<NativeDocuments>().on_export(move || {
            let (epoch, review) = {
                let s = state.borrow();
                if s.documents_pending {
                    return;
                }
                (s.epoch.current(), s.document_reviews.download.clone())
            };
            let Some(review) = review else { return };
            let result = crate::picker::save(&review.name);
            if !state.borrow().epoch.accepts(epoch) {
                return;
            }
            match result {
                Ok(Some(path)) => enqueue(&state, &tx, Action::Export(review, path.into())),
                Ok(None) => (),
                Err(e) => fail(&state, e),
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.global::<NativeDocuments>().on_generate(move || {
            let Some(w) = weak.upgrade() else { return };
            let ui = w.global::<NativeDocuments>();
            let headers: Vec<_> = ui.get_headers().split('\t').map(str::to_owned).collect();
            let rows: Vec<Vec<Value>> = ui
                .get_rows()
                .lines()
                .map(|line| {
                    line.split('\t')
                        .map(|v| json!({"type":"text","value":v}))
                        .collect()
                })
                .collect();
            if headers.iter().any(|v| v.is_empty())
                || headers.len() > 100
                || rows.len() > 1000
                || rows.iter().any(|r| r.len() != headers.len())
            {
                fail(
                    &state,
                    "Use 1–100 named columns and at most 1,000 rows with matching cell counts.",
                );
                return;
            }
            let format = ["csv", "markdown", "xlsx", "pdf", "pptx"]
                .get(ui.get_format_index() as usize)
                .unwrap_or(&"csv");
            enqueue(
                &state,
                &tx,
                Action::Generate(
                    ui.get_name().into(),
                    (*format).into(),
                    json!({"headers":headers,"rows":rows}),
                ),
            );
        });
    }
}
pub(super) fn project(window: &MainWindow, state: &UiState, old: &View) {
    let ui = window.global::<NativeDocuments>();
    let d = &state.view.documents;
    ui.set_available(state.view.available);
    ui.set_busy(state.documents_pending || !state.view.available);
    ui.set_pending(!d.pending.is_null() && d.draft_identity == state.view.draft.identity);
    ui.set_document(
        d.pending["allowed_delivery"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == "context")),
    );
    ui.set_image(
        d.pending["allowed_delivery"]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == "reference_only")),
    );
    ui.set_review(
        if d.pending.is_null() {
            String::new()
        } else {
            format!(
                "{} · {} bytes\n{}\nSHA-256 {}\n{}",
                d.pending["name"].as_str().unwrap_or("Selected file"),
                d.pending["byte_len"],
                if d.pending["content"]["kind"] == "image" {
                    "Reference only: this text model will not receive image pixels."
                } else if d.pending["content"]["truncated"] == true {
                    "A bounded excerpt will be attached; the complete original is retained."
                } else {
                    "The validated document excerpt will be attached to your next message."
                },
                d.pending["sha256"].as_str().unwrap_or(""),
                d.pending["content"]["context"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(600)
                    .collect::<String>()
            )
        }
        .into(),
    );
    let count = state.view.draft.attachments.len() + state.view.draft.media.len();
    let text = if count == 0 {
        String::new()
    } else {
        format!("{count} saved attachment(s) for this draft")
    };
    ui.set_attached(text.clone().into());
    window.set_attachment_name(text.into());
    window.set_attachment_path("".into());
    ui.set_opened(!d.preview_path.is_empty() && state.view.available);
    ui.set_can_save_answer(state.document_reviews.answer.is_some() && state.view.available);
    if d.preview_id != old.documents.preview_id || d.preview_text != old.documents.preview_text {
        ui.set_generator_open(false);
        window.set_artifacts_viewer_title(d.preview_title.clone().into());
        window.set_artifacts_viewer_text(d.preview_text.clone().into());
        window.set_artifacts_viewer_artifact_id(d.preview_id.clone().into());
        window.set_artifacts_viewer_is_image(false);
        window.set_artifacts_viewer_open(!d.preview_id.is_empty());
    }
    window.set_artifacts_viewer_opening(state.documents_pending);
    window.set_artifacts_status(
        if state.documents_pending {
            "Working with local document…"
        } else {
            ""
        }
        .into(),
    );
    window.set_composer_artifacts(window.get_artifacts_list());
}
