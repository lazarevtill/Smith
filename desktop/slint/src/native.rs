use crate::{
    bridge::{Actions, Bridge, Chat, Config, Intent, NavigationEpoch, Transport, View},
    fixture::Fixture,
    navigation::{Navigation, Place},
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

slint::include_modules!();
#[path = "native_agents.rs"]
mod agents_ui;
#[path = "native_catalog.rs"]
mod catalog_ui;
#[path = "native_documents.rs"]
mod documents_ui;
#[path = "native_extensions.rs"]
mod extensions_ui;
#[path = "native_historical.rs"]
mod historical_ui;
#[path = "native_memory.rs"]
mod memory_ui;
#[path = "native_message_cards.rs"]
mod message_cards_ui;
#[path = "native_mlx.rs"]
mod mlx_ui;
#[path = "native_profile.rs"]
mod profile_ui;
#[path = "native_settings.rs"]
mod settings_ui;
#[path = "native_tasks.rs"]
mod tasks_ui;
#[path = "native_transcription.rs"]
mod transcription_ui;
include!(concat!(env!("OUT_DIR"), "/unavailable.rs"));

enum Command {
    Initialize,
    New,
    Home,
    Open(String),
    Send(Intent, String),
    Regenerate(Intent, String),
    Branch(Intent, String),
    Feedback(crate::messages::FeedbackReview, String),
    Cancel,
    Refresh,
    SelectModel(Config, String),
    DraftChanged(u64, String),
    DiscardDraft,
    Edit(Intent, String, String),
    Rename(Chat, Config, String),
    Trash(Vec<Chat>, Actions, bool),
    Batch(crate::batch::BatchReview),
    Reverse(Actions, bool),
    Flush(mpsc::Sender<crate::bridge::Result<()>>),
    Settings(settings_ui::Action),
    Documents(documents_ui::Action),
    Mlx(mlx_ui::Action),
    Tasks(tasks_ui::Action),
    Profile(profile_ui::Action),
    Agents(agents_ui::Action),
    Extensions(extensions_ui::Action),
    Catalog(catalog_ui::Action),
    Transcription(transcription_ui::Action),
    Memory(u64, memory_ui::Action),
    Historical(u64, historical_ui::Action),
    Original(u64, message_cards_ui::Action),
    Stop,
}
struct Update {
    original_response: Option<(
        u64,
        crate::bridge::Result<crate::historical_source_bridge::HistoricalMessage>,
    )>,
    epoch: u64,
    view: View,
    intent: Option<Intent>,
    error: Option<String>,
    sent: Option<String>,
    edit_sent: bool,
    settings_response: Option<(bool, bool)>,
    command_response: bool,
    documents_response: bool,
    mlx_response: Option<bool>,
    tasks_response: bool,
    memory_response: Option<bool>,
    memory_generation: u64,
    memory_panel: memory_ui::Panel,
    historical_panel: historical_ui::Panel,
    historical_generation: u64,
    historical_response: bool,
    transcription_response: bool,
    transcription_panel: transcription_ui::Panel,
    agents_response: Option<(bool, bool)>,
    agent_panel: agents_ui::Panel,
    extension_panel: extensions_ui::Panel,
    catalog_panel: catalog_ui::Panel,
    extensions_response: Option<(bool, bool)>,
    catalog_response: Option<(bool, bool)>,
    batch_response: Option<bool>,
    batch_context: Option<crate::batch::BatchContext>,
    profile_response: bool,
    profile_panel: profile_ui::Panel,
    task_panel: tasks_ui::Panel,
    feedback_reviews: std::collections::BTreeMap<String, crate::messages::FeedbackReview>,
    document_reviews: documents_ui::Reviews,
}
#[derive(Default)]
struct UiState {
    original_message: Option<crate::historical_source_bridge::HistoricalMessage>,
    original_generation: u64,
    original_pending: bool,
    original_screen_generation: Rc<std::cell::Cell<u64>>,
    original_seen_screen_generation: u64,
    epoch: NavigationEpoch,
    view: View,
    intent: Option<Intent>,
    session_query: String,
    model_query: String,
    draft_seen: String,
    editing: Option<(Intent, String)>,
    navigation: Navigation,
    pending_navigation: Option<(usize, Place)>,
    screen: i32,
    trash: bool,
    settings_target: Option<Config>,
    settings_revision: Option<u64>,
    settings_pending: bool,
    settings_requested: bool,
    probe_identity: String,
    load_identity: String,
    last_error: Option<String>,
    documents_pending: bool,
    export_pending: bool,
    export_notice: Option<String>,
    mlx_pending: bool,
    mlx_target: Option<Config>,
    mlx_revision: Option<u64>,
    mlx_identity: String,
    task_panel: tasks_ui::Panel,
    tasks_pending: bool,
    task_proposal_identity: String,
    task_approval_identity: String,
    profile_panel: profile_ui::Panel,
    profile_pending: bool,
    profile_visible: bool,
    agent_panel: agents_ui::Panel,
    extension_panel: extensions_ui::Panel,
    catalog_panel: catalog_ui::Panel,
    extensions_pending: bool,
    extensions_requested: bool,
    catalog_requested: bool,
    catalog_pending: bool,
    catalog_replacement: Option<crate::catalog_bridge::EntryReview>,
    catalog_setup_engine: Option<i32>,
    agent_draft: agents_ui::Draft,
    agents_pending: bool,
    agents_requested: bool,
    memory_pending: bool,
    memory_generation: u64,
    memory_filter: Option<crate::memory_bridge::MemoryFilter>,
    memory_panel: memory_ui::Panel,
    historical_panel: historical_ui::Panel,
    historical_generation: u64,
    historical_pending: bool,
    historical_filter: Option<(i32, crate::historical_memory_bridge::HistoricalFilter)>,
    memory_selected: Option<crate::memory_bridge::MemoryReview>,
    transcription_pending: bool,
    transcription_identity: String,
    transcription_panel: transcription_ui::Panel,
    batch_mode: bool,
    batch_pending: bool,
    batch: crate::batch::Selection,
    batch_context: Option<crate::batch::BatchContext>,
    task_seen_id: String,
    task_run_identity: String,
    feedback_reviews: std::collections::BTreeMap<String, crate::messages::FeedbackReview>,
    document_reviews: documents_ui::Reviews,
}
pub struct Driver {
    tx: mpsc::Sender<(u64, Command)>,
    rx: mpsc::Receiver<Update>,
    state: Rc<RefCell<UiState>>,
    worker: Option<thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.tx.send((0, Command::Stop));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Driver {
    pub fn attach(window: &MainWindow, fixture: Fixture) -> Self {
        Self::attach_transport(window, fixture.transport())
    }
    pub fn attach_transport<T: Transport>(window: &MainWindow, transport: T) -> Self {
        Self::attach_worker(window, transport, profile_ui::Worker::new(None).unwrap())
    }
    /// Explicit chooser injection for owned callback qualification. Normal
    /// constructors always use the native save dialog; there is no environment
    /// or command-line switch. The chooser runs only on the UI thread after the
    /// original Export action and cannot bypass review or exclusive creation.
    pub fn attach_transport_with_export_picker<T: Transport>(
        window: &MainWindow,
        transport: T,
        choose: impl Fn() -> crate::bridge::Result<Option<String>> + 'static,
    ) -> Self {
        let driver = Self::attach_transport(window, transport);
        documents_ui::wire_export(window, &driver.state, &driver.tx, choose);
        driver
    }
    pub fn attach_transport_with_profile<T: Transport>(
        window: &MainWindow,
        transport: T,
        root: &std::path::Path,
    ) -> crate::bridge::Result<Self> {
        Ok(Self::attach_worker(
            window,
            transport,
            profile_ui::Worker::new(Some(
                &root
                    .canonicalize()
                    .map_err(|_| "Owned workspace path unavailable.")?,
            ))?,
        ))
    }
    fn attach_worker<T: Transport>(
        window: &MainWindow,
        transport: T,
        mut profile: profile_ui::Worker,
    ) -> Self {
        let (tx, commands) = mpsc::channel();
        let (updates, rx) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        let worker = thread::spawn(move || {
            let mut bridge = Bridge::new(transport);
            let mut epoch = 0;
            let mut task_panel = tasks_ui::Panel::default();
            let mut agent_panel = agents_ui::Panel::default();
            let mut extension_panel = extensions_ui::Panel::default();
            let mut catalog_panel = catalog_ui::Panel::default();
            let mut transcription = transcription_ui::Worker::default();
            let mut memory_panel = memory_ui::Panel::default();
            let mut memory_generation = 0;
            let mut historical_panel = historical_ui::Panel::default();
            let mut historical_generation = 0;
            loop {
                if worker_stopping.load(Ordering::Acquire) {
                    break;
                }
                let command = commands.recv_timeout(Duration::from_millis(250));
                if worker_stopping.load(Ordering::Acquire) {
                    break;
                }
                let mut sent = None;
                let mut edit_sent = false;
                let mut settings_response = None;
                let mut documents_response = false;
                let mut mlx_response = None;
                let mut tasks_response = false;
                let mut profile_response = false;
                let mut batch_response = None;
                let mut agents_response = None;
                let mut extensions_response = None;
                let mut catalog_response = None;
                let mut transcription_response = false;
                let mut memory_response = None;
                let mut historical_response = false;
                let mut original_response = None;
                let command_response = command.is_ok();
                let result = match command {
                    Ok((_, Command::Stop)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok((next, command)) => {
                        epoch = next;
                        match command {
                            Command::Initialize => bridge.initialize(),
                            Command::New => bridge.new_chat(),
                            Command::Home => bridge.home(),
                            Command::Open(id) => bridge.open(&id),
                            Command::Send(intent, text) => {
                                let result = bridge.send(&intent, &text);
                                if result.is_ok() {
                                    sent = Some(text);
                                }
                                result
                            }
                            Command::Regenerate(intent, id) => bridge.regenerate(&intent, &id),
                            Command::Branch(intent, id) => bridge.branch(&intent, &id),
                            Command::Feedback(review, kind) => bridge.feedback(&review, &kind),
                            Command::Cancel => bridge.cancel(),
                            Command::Refresh => bridge.refresh_lists(),
                            Command::SelectModel(config, model) => {
                                bridge.select_model(&config, &model)
                            }
                            Command::DraftChanged(identity, text) => {
                                bridge.change_draft(identity, text)
                            }
                            Command::DiscardDraft => bridge.discard_and_reload_draft(),
                            Command::Edit(intent, id, text) => {
                                let result = bridge.edit(&intent, &id, &text);
                                edit_sent = result.is_ok();
                                result
                            }
                            Command::Rename(chat, config, title) => {
                                bridge.rename(&chat, &config, &title)
                            }
                            Command::Trash(chats, history, restore) => {
                                bridge.trash(&chats, &history, restore)
                            }
                            Command::Batch(review) => {
                                let result = bridge.batch_apply(&review);
                                batch_response = Some(result.is_ok());
                                result
                            }
                            Command::Reverse(history, redo) => bridge.reverse(&history, redo),
                            Command::Flush(reply) => {
                                let result = bridge.save_draft();
                                let _ = reply.send(result.clone());
                                result
                            }
                            Command::Original(generation, action) => {
                                let result = action.execute(&mut bridge);
                                let error = result.as_ref().err().cloned();
                                original_response = Some((generation, result));
                                error.map_or(Ok(()), Err)
                            }
                            Command::Historical(generation, action) => {
                                historical_generation = generation;
                                historical_response = true;
                                action.execute(&mut bridge, &mut historical_panel)
                            }
                            Command::Memory(generation, action) => {
                                memory_generation = generation;
                                memory_response = Some(action.changes_editor());
                                action.execute(&mut bridge, &mut memory_panel)
                            }
                            Command::Transcription(action) => {
                                transcription_response = true;
                                transcription.execute(&mut bridge, action)
                            }
                            Command::Catalog(action) => {
                                catalog_response = Some((
                                    matches!(&action, catalog_ui::Action::Details(_)),
                                    matches!(&action, catalog_ui::Action::Setup(..)),
                                ));
                                action.execute(&mut bridge, &mut catalog_panel)
                            }
                            Command::Extensions(action) => {
                                extensions_response = Some((action.populates(), action.toggles()));
                                action.execute(&mut bridge, &mut extension_panel)
                            }
                            Command::Agents(action) => {
                                agents_response = Some((action.populates(), action.launches()));
                                action.execute(&mut bridge, &mut agent_panel, &mut task_panel)
                            }
                            Command::Profile(action) => {
                                profile_response = true;
                                profile.execute(&mut bridge, action)
                            }
                            Command::Tasks(action) => {
                                tasks_response = true;
                                action.execute(&mut bridge, &mut task_panel)
                            }
                            Command::Mlx(action) => {
                                mlx_response = Some(matches!(&action, mlx_ui::Action::Refresh));
                                action.execute(&mut bridge)
                            }
                            Command::Documents(action) => {
                                documents_response = true;
                                action.execute(&mut bridge)
                            }
                            Command::Settings(action) => {
                                settings_response =
                                    Some((action.replaces_fields(), action.adopts_revision()));
                                action.execute(&mut bridge)
                            }
                            Command::Stop => unreachable!(),
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if profile.active() {
                            profile.poll(&mut bridge)
                        } else if bridge.view.available {
                            let chat = bridge.poll();
                            let task = task_panel.poll(&mut bridge);
                            let transcript = transcription.poll(&mut bridge);
                            let catalog = catalog_panel.poll(&mut bridge);
                            chat.and(task).and(transcript).and(catalog)
                        } else {
                            continue;
                        }
                    }
                };
                if !bridge.view.available {
                    task_panel = Default::default();
                    agent_panel = Default::default();
                    extension_panel = Default::default();
                    catalog_panel = Default::default();
                    transcription.clear();
                    memory_panel = Default::default();
                    historical_panel = Default::default();
                }
                let update = Update {
                    original_response,
                    epoch,
                    view: bridge.view.clone(),
                    intent: bridge.intent().ok(),
                    error: result.err(),
                    sent,
                    edit_sent,
                    settings_response,
                    command_response,
                    documents_response,
                    mlx_response,
                    tasks_response,
                    memory_response,
                    memory_generation,
                    memory_panel: memory_panel.clone(),
                    historical_panel: historical_panel.clone(),
                    historical_generation,
                    historical_response,
                    transcription_response,
                    transcription_panel: transcription.panel.clone(),
                    agents_response,
                    agent_panel: agent_panel.clone(),
                    extension_panel: extension_panel.clone(),
                    catalog_panel: catalog_panel.clone(),
                    catalog_response,
                    extensions_response,
                    batch_response,
                    batch_context: bridge.batch_context(),
                    profile_response,
                    profile_panel: profile.panel.clone(),
                    task_panel: task_panel.clone(),
                    feedback_reviews: bridge
                        .view
                        .conversation
                        .as_ref()
                        .map(|c| {
                            c.active_messages
                                .iter()
                                .filter_map(|m| {
                                    bridge
                                        .feedback_review(&m.id)
                                        .ok()
                                        .map(|r| (m.id.clone(), r))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    document_reviews: documents_ui::Reviews::capture(&bridge),
                };
                if updates.send(update).is_err() {
                    break;
                }
            }
        });
        let state = Rc::new(RefCell::new(UiState::default()));
        let platform = crate::platform_ui::Platform::current();
        let platform_ui = window.global::<NativePlatform>();
        platform_ui.set_mlx(platform.mlx());
        platform_ui.set_owned_runtime(platform.owned_runtime());
        platform_ui.set_pickers(platform.pickers());
        platform_ui.set_clipboard(platform.pickers());
        platform_ui.set_engines(strings(
            platform.engines().into_iter().map(str::to_owned).collect(),
        ));
        wire_unavailable(window);
        settings_ui::wire(window, &state, &tx);
        profile_ui::wire(window, &state, &tx);
        agents_ui::wire(window, &state, &tx);
        extensions_ui::wire(window, &state, &tx);
        catalog_ui::wire(window, &state, &tx);
        transcription_ui::wire(window, &state, &tx);
        memory_ui::wire(window, &state, &tx);
        historical_ui::wire(window, &state, &tx);
        message_cards_ui::wire(window, &state, &tx);
        documents_ui::wire(window, &state, &tx);
        mlx_ui::wire(window, &state, &tx);
        {
            let tx = tx.clone();
            let state = state.clone();
            window.on_set_default_model(move |model| {
                let s = state.borrow();
                if s.view.available && s.view.models.iter().any(|m| m == model.as_str()) {
                    let _ = tx.send((
                        s.epoch.current(),
                        Command::SelectModel(s.view.config.clone(), model.into()),
                    ));
                }
            });
        }
        window.global::<Palette>().set_dark(false);
        window.set_appearance_modes(ModelRc::new(VecModel::from(vec![SharedString::from(
            "Light",
        )])));
        window.set_appearance("Light".into());
        window.set_frameless(false);
        window.set_show_welcome(false);
        window.set_session_loading(true);
        window.set_server_address("Local workspace".into());
        window.set_server_status("checking".into());
        window.set_status_text("Opening local workspace…".into());
        window.set_app_version("Native bridge qualification".into());
        window.set_exec_providers(ModelRc::new(VecModel::from(vec![SharedString::from(
            "Unknown",
        )])));
        window.set_execution_provider("Unknown".into());
        window.set_models_dir("Not connected in native qualification".into());
        window.set_artifacts_dir("Backend-owned documents".into());
        {
            let tx = tx.clone();
            let state = state.clone();
            let weak = window.as_weak();
            window.on_new_session(move || {
                let mut s = state.borrow_mut();
                if !before_navigation(&weak, &tx, &mut s) {
                    return;
                }
                s.screen = 0;
                if let Some(w) = weak.upgrade() {
                    w.set_active_view(0);
                }
                let epoch = s.epoch.advance();
                if let Some(w) = weak.upgrade() {
                    w.set_session_loading(true);
                }
                let _ = tx.send((epoch, Command::New));
            });
        }
        {
            let tx = tx.clone();
            let state = state.clone();
            let weak = window.as_weak();
            window.on_select_session(move |index| {
                let mut s = state.borrow_mut();
                if s.trash {
                    return;
                }
                if !before_navigation(&weak, &tx, &mut s) {
                    return;
                }
                let chats = filtered_chats(&s);
                if let Some(chat) = usize::try_from(index).ok().and_then(|i| chats.get(i)) {
                    let id = chat.id.clone();
                    let epoch = s.epoch.advance();
                    if let Some(w) = weak.upgrade() {
                        w.set_session_loading(true);
                    }
                    let _ = tx.send((epoch, Command::Open(id)));
                }
            });
        }
        {
            let tx = tx.clone();
            let state = state.clone();
            let weak = window.as_weak();
            window.on_send(move |text| {
                let mut s = state.borrow_mut();
                if let Some((intent, message)) = s.editing.clone() {
                    if let Some(w) = weak.upgrade() {
                        w.set_chat_operation("Saving edit…".into());
                    }
                    let _ = tx.send((
                        s.epoch.current(),
                        Command::Edit(intent, message, text.to_string()),
                    ));
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    capture_draft(&w, &tx, &mut s);
                }
                if let Some(intent) = s.intent.clone() {
                    if let Some(w) = weak.upgrade() {
                        w.set_chat_operation("Sending…".into());
                    }
                    let _ = tx.send((s.epoch.current(), Command::Send(intent, text.to_string())));
                } else if let Some(w) = weak.upgrade() {
                    w.set_status_text(
                        "Choose New chat before sending in this qualification build.".into(),
                    );
                }
            });
        }
        {
            let tx = tx.clone();
            let state = state.clone();
            window.on_stop(move || {
                let _ = tx.send((state.borrow().epoch.current(), Command::Cancel));
            });
        }
        {
            let tx = tx.clone();
            let state = state.clone();
            window.on_regenerate(move |id| {
                let s = state.borrow();
                if let Some(intent) = s.intent.clone() {
                    let _ = tx.send((
                        s.epoch.current(),
                        Command::Regenerate(intent, id.to_string()),
                    ));
                }
            });
        }
        {
            let tx = tx.clone();
            let state = state.clone();
            window.on_switch_branch(move |id, direction| {
                let s = state.borrow();
                let Some(c) = &s.view.conversation else {
                    return;
                };
                let Some(m) = c.active_messages.iter().find(|m| m.id == id.as_str()) else {
                    return;
                };
                let index = m.branch_index as i64 - 1 + direction as i64;
                if let (Some(target), Some(intent)) = (
                    usize::try_from(index).ok().and_then(|i| m.siblings.get(i)),
                    s.intent.clone(),
                ) {
                    let _ = tx.send((s.epoch.current(), Command::Branch(intent, target.clone())));
                }
            });
        }
        {
            let state = state.clone();
            let weak = window.as_weak();
            window.on_filter_sessions(move |query| {
                let mut s = state.borrow_mut();
                s.session_query = query.to_string();
                if let Some(w) = weak.upgrade() {
                    project_sessions(&w, &s);
                }
            });
        }
        {
            let state = state.clone();
            let weak = window.as_weak();
            window.on_search_models(move |query| {
                let mut s = state.borrow_mut();
                s.model_query = query.to_string();
                if let Some(w) = weak.upgrade() {
                    project_models(&w, &s);
                }
            });
        }
        for register in [
            MainWindow::on_refresh_models,
            MainWindow::on_artifacts_refresh,
            MainWindow::on_activity_refresh,
        ] {
            let tx = tx.clone();
            let state = state.clone();
            register(window, move || {
                let _ = tx.send((state.borrow().epoch.current(), Command::Refresh));
            });
        }
        {
            let state = state.clone();
            let weak = window.as_weak();
            window.on_activity_select_task(move |id| {
                let s = state.borrow();
                if let Some(task) = s.view.tasks.iter().find(|t| t["id"] == id.as_str()) {
                    if let Some(w) = weak.upgrade() {
                        w.set_activity_selected_task(id);
                        w.set_activity_detail(task_item(task));
                        w.set_activity_has_selection(true);
                    }
                }
            });
        }
        {
            let weak = window.as_weak();
            window.on_setting_changed(move || {
                if let Some(w) = weak.upgrade() {
                    w.global::<Palette>().set_dark(false);
                    w.set_appearance("Light".into());
                    w.set_status_text(
                        "Light appearance is active. This setting is not available yet.".into(),
                    );
                }
            });
        }
        {
            let state = state.clone();
            let tx = tx.clone();
            window.on_message_feedback(move |id, kind| {
                let s = state.borrow();
                if let Some(review) = s.feedback_reviews.get(id.as_str()) {
                    let _ = tx.send((
                        s.epoch.current(),
                        Command::Feedback(
                            review.clone(),
                            if s.view
                                .conversation
                                .as_ref()
                                .and_then(|c| {
                                    c.active_messages.iter().find(|m| m.id == id.as_str())
                                })
                                .is_some_and(|m| m.feedback == kind.as_str())
                            {
                                String::new()
                            } else {
                                kind.to_string()
                            },
                        ),
                    ));
                }
            });
        }
        {
            let state = state.clone();
            window.on_copy_message(move |text| {
                if let Err(e) = crate::picker::copy_text(text.as_str()) {
                    state.borrow_mut().last_error = Some(e);
                }
            });
        }
        {
            let state = state.clone();
            window.on_save_code(move |text, language| {
                let extension = match language.as_str() {
                    "rust" => "rs",
                    "python" => "py",
                    "javascript" => "js",
                    "typescript" => "ts",
                    "json" => "json",
                    "html" => "html",
                    "css" => "css",
                    "sql" => "sql",
                    "bash" | "sh" => "sh",
                    _ => "txt",
                };
                let result = crate::picker::save(&format!("code.{extension}")).and_then(|path| {
                    if let Some(path) = path {
                        crate::documents::write_new(std::path::Path::new(&path), text.as_bytes())
                    } else {
                        Ok(())
                    }
                });
                if let Err(e) = result {
                    state.borrow_mut().last_error = Some(e);
                }
            });
        }
        window.on_filter_slash(|_| {});
        wire_management(window, &state, &tx);
        wire_batch(window, &state, &tx);
        {
            let tx = tx.clone();
            let state = state.clone();
            window.on_select_model(move |model| {
                let s = state.borrow();
                let _ = tx.send((
                    s.epoch.current(),
                    Command::SelectModel(s.view.config.clone(), model.to_string()),
                ));
            });
        }
        window.on_window_close(|| {
            let _ = slint::quit_event_loop();
        });
        {
            let weak = window.as_weak();
            window.on_window_minimize(move || {
                if let Some(w) = weak.upgrade() {
                    w.window().set_minimized(true);
                }
            });
        }
        {
            let weak = window.as_weak();
            window.on_window_maximize(move || {
                if let Some(w) = weak.upgrade() {
                    w.window().set_maximized(!w.window().is_maximized());
                }
            });
        }
        window.on_window_drag(|| {}); // OS-native frame owns dragging.
        tasks_ui::wire(window, &state, &tx);
        tx.send((0, Command::Initialize)).unwrap();
        Self {
            tx,
            rx,
            state,
            worker: Some(worker),
            stopping,
        }
    }
    /// Called on the UI thread; safe for both the event-loop timer and the
    /// software-renderer harness. No network or storage work runs here.
    pub fn pump(&self, window: &MainWindow) {
        {
            let mut state = self.state.borrow_mut();
            capture_draft(window, &self.tx, &mut state);
            memory_ui::capture_filter(window, &mut state);
            historical_ui::capture_filter(window, &mut state);
            if state.view.available && window.get_active_view() == 1 && !state.catalog_requested {
                state.catalog_requested = true;
                state.catalog_pending = true;
                let _ = self.tx.send((
                    state.epoch.current(),
                    Command::Catalog(catalog_ui::Action::Refresh(
                        window.global::<NativeCatalog>().get_trash(),
                    )),
                ));
            }
            let profile_visible = window.get_active_view() == 2 && window.get_settings_tab() == 8;
            if state.profile_visible && !profile_visible {
                profile_ui::clear_keys(window);
                window.global::<NativeProfile>().set_confirmed(false);
                let _ = self.tx.send((
                    state.epoch.current(),
                    Command::Profile(profile_ui::Action::Discard),
                ));
            }
            state.profile_visible = profile_visible;
            if state.view.available
                && window.get_active_view() == 3
                && window.get_autonomy_sub_view() == 4
                && !state.extensions_requested
            {
                state.extensions_requested = true;
                state.extensions_pending = true;
                let _ = self.tx.send((
                    state.epoch.current(),
                    Command::Extensions(extensions_ui::Action::Refresh),
                ));
            }
            if state.view.available && window.get_active_view() == 3 && !state.agents_requested {
                state.agents_requested = true;
                state.agents_pending = true;
                let _ = self.tx.send((
                    state.epoch.current(),
                    Command::Agents(agents_ui::Action::Refresh),
                ));
            }
            if state.view.available && window.get_active_view() == 2 && !state.settings_requested {
                state.settings_requested = true;
                state.settings_pending = true;
                let _ = self.tx.send((
                    state.epoch.current(),
                    Command::Settings(settings_ui::Action::Reload),
                ));
            }
            let screen_generation = state.original_screen_generation.get();
            if screen_generation != state.original_seen_screen_generation {
                state.original_seen_screen_generation = screen_generation;
                message_cards_ui::invalidate(&mut state);
                project(window, &state, &View::default());
            }
            if state.view.available
                && state.pending_navigation.is_none()
                && window.get_active_view() != state.screen
            {
                message_cards_ui::invalidate(&mut state);
                project(window, &state, &View::default());
                state.screen = window.get_active_view();
                let place = Place {
                    screen: state.screen,
                    chat: state.view.conversation.as_ref().map(|c| c.chat.id.clone()),
                };
                state.navigation.record(place);
            }
        }
        while let Ok(update) = self.rx.try_recv() {
            let mut state = self.state.borrow_mut();
            if update.view.available
                && update
                    .original_response
                    .as_ref()
                    .is_some_and(|(generation, _)| *generation != state.original_generation)
            {
                continue;
            }
            if !state.epoch.accepts(update.epoch)
                && update.view.available
                && !update.profile_response
            {
                // A completed old-screen command cannot replace the new view,
                // but its single-flight UI reservation must still be released.
                if update.settings_response.is_some() {
                    state.settings_pending = false;
                }
                if update.mlx_response.is_some() {
                    state.mlx_pending = false;
                }
                if update.documents_response {
                    state.documents_pending = false;
                    state.export_pending = false;
                    window.set_export_session_saving(false);
                }
                if update.historical_response
                    && update.historical_generation == state.historical_generation
                {
                    state.historical_pending = false;
                }
                if update.memory_response.is_some()
                    && update.memory_generation == state.memory_generation
                {
                    state.memory_pending = false;
                }
                if update.transcription_response {
                    state.transcription_pending = false;
                }
                if update.extensions_response.is_some() {
                    state.extensions_pending = false;
                }
                if update.agents_response.is_some() {
                    state.agents_pending = false;
                }
                if update.batch_response.is_some() {
                    state.batch_pending = false;
                }
                if update.tasks_response {
                    state.tasks_pending = false;
                }
                if update.catalog_response.is_some() {
                    state.catalog_pending = false;
                }
                continue;
            }
            let mut original_changed = false;
            if update.command_response {
                state.export_notice = None;
            }
            if !update.view.available {
                if state.view.available {
                    state.epoch.advance();
                }
                state.navigation.clear();
                state.batch.clear();
                state.batch_context = None;
                state.batch_mode = false;
                state.batch_pending = false;
                state.editing = None;
                state.draft_seen.clear();
                state.settings_target = None;
                state.settings_revision = None;
                state.settings_pending = false;
                state.settings_requested = false;
                mlx_ui::clear(window, &mut state);
                tasks_ui::clear(window, &mut state);
                agents_ui::clear(window, &mut state);
                extensions_ui::clear(window, &mut state);
                catalog_ui::clear(window, &mut state);
                transcription_ui::clear(window, &mut state);
                memory_ui::clear(window, &mut state);
                historical_ui::clear(window, &mut state);
                message_cards_ui::invalidate(&mut state);
                original_changed = true;
                profile_ui::clear_keys(window);
                state.documents_pending = false;
                state.export_pending = false;
                state.export_notice = None;
                window.set_export_session_saving(false);
                state.document_reviews = Default::default();
                window.global::<NativeDocuments>().set_name("".into());
                window.global::<NativeDocuments>().set_headers("".into());
                window.global::<NativeDocuments>().set_rows("".into());
                window.global::<NativeSettings>().set_model_id("".into());
                window.global::<NativeSettings>().set_context("".into());
                window.global::<NativeSettings>().set_threads("".into());
                window.global::<NativeSettings>().set_layers("".into());
                let settings = window.global::<NativeSettings>();
                settings.set_device("".into());
                settings.set_output("".into());
                settings.set_prefill_threads("".into());
                settings.set_batch("".into());
                settings.set_microbatch("".into());
                settings.set_host_budget("".into());
                settings.set_device_budget("".into());
                settings.set_headroom("".into());
                settings.set_kv_index(0);
                settings.set_flash_index(0);
                settings.set_mmap_index(0);
                settings.set_mlock_index(0);
                window.global::<NativeSettings>().set_token("".into());
                window.global::<NativeSettings>().set_endpoint("".into());
                window.global::<NativeSettings>().set_executable("".into());
                window.global::<NativeSettings>().set_model("".into());
                window.global::<NativeSettings>().set_source("".into());
                window
                    .global::<NativeSettings>()
                    .set_source_revision("".into());
                window.set_composer_text("".into());
                window.set_attachment_name("".into());
                window.set_attachment_path("".into());
                window.set_pending_artifact_id("".into());
                window.set_pending_artifact_title("".into());
                window.set_artifacts_viewer_text("".into());
                window.set_artifacts_viewer_image(slint::Image::default());
                window.set_artifacts_viewer_open(false);
                window.set_activity_has_selection(false);
                window.set_activity_detail(ActivityItem::default());
            }
            if let Some(sent) = update.sent {
                if window.get_composer_text().as_str() == sent {
                    window.set_composer_text("".into());
                    state.draft_seen.clear();
                }
            }
            let old = state.view.clone();
            state.view = update.view;
            if state
                .original_message
                .as_ref()
                .is_some_and(|p| p.view(&state.view, &state.epoch).is_err())
            {
                message_cards_ui::invalidate(&mut state);
                original_changed = true;
            }
            if let Some((generation, result)) = update.original_response {
                if generation == state.original_generation {
                    state.original_pending = false;
                    state.original_message = result
                        .ok()
                        .filter(|p| p.view(&state.view, &state.epoch).is_ok());
                    original_changed = true;
                }
            }
            state.intent = update.intent;
            state.document_reviews = update.document_reviews;
            state.feedback_reviews = update.feedback_reviews;
            state.batch_context = update.batch_context;
            if let Some(success) = update.batch_response {
                state.batch_pending = false;
                if success {
                    state.batch.clear();
                    project_sessions(window, &state);
                }
            }
            if update.memory_generation == state.memory_generation {
                state.memory_panel = update.memory_panel;
                if let Some(changes_editor) = update.memory_response {
                    state.memory_pending = false;
                    if changes_editor && update.error.is_none() {
                        state.memory_selected = None;
                        window.global::<NativeMemory>().set_editor_open(false);
                        window.global::<NativeMemory>().set_editing(false);
                        window.global::<NativeMemory>().set_fact("".into());
                        window.global::<NativeMemory>().set_tags("".into());
                    }
                }
            }
            if update.historical_generation == state.historical_generation {
                state.historical_panel = update.historical_panel;
                if update.historical_response {
                    state.historical_pending = false;
                }
            }
            state.task_panel = update.task_panel;
            state.transcription_panel = update.transcription_panel;
            if update.transcription_response {
                state.transcription_pending = false;
            }
            state.catalog_panel = update.catalog_panel;
            if let Some((details, setup)) = update.catalog_response {
                state.catalog_pending = false;
                catalog_ui::received(window, &mut state, details, setup, update.error.is_none());
            }
            state.extension_panel = update.extension_panel;
            if let Some((populate, toggled)) = update.extensions_response {
                state.extensions_pending = false;
                if update.error.is_none() {
                    if populate {
                        extensions_ui::populate(window, &state);
                    }
                    if toggled {
                        extensions_ui::enabled(window, &state);
                    }
                }
            }
            state.agent_panel = update.agent_panel;
            if let Some((populate, launch)) = update.agents_response {
                state.agents_pending = false;
                if update.error.is_none() {
                    if populate {
                        agents_ui::populate(window, &mut state);
                    }
                    if launch {
                        window.set_active_view(6);
                        window.set_activity_sub_view(0);
                    }
                }
            }
            if update.profile_response
                || (state.profile_panel.review.is_none() && update.profile_panel.review.is_some())
            {
                window.global::<NativeProfile>().set_confirmed(false);
            }
            state.profile_panel = update.profile_panel;
            if update.profile_response {
                state.profile_pending = false;
            }
            if update.tasks_response {
                state.tasks_pending = false;
            }
            if let Some(populate) = update.mlx_response {
                state.mlx_pending = false;
                if update.error.is_none() && populate {
                    mlx_ui::populate(window, &mut state);
                }
            }
            if update.documents_response {
                state.documents_pending = false;
                if state.export_pending {
                    state.export_pending = false;
                    window.set_export_session_saving(false);
                    if update.error.is_none() {
                        state.export_notice = Some("Conversation exported.".into());
                    }
                }
            }
            if let Some((replace_fields, adopt_revision)) = update.settings_response {
                state.settings_pending = false;
                if update.error.is_none() && adopt_revision {
                    state.settings_target = Some(state.view.config.clone());
                    state.settings_revision =
                        state.view.settings.setup["preferences"]["revision"].as_u64();
                    if replace_fields {
                        settings_ui::populate(window, &mut state);
                    }
                }
            }
            if state.view.available
                && (old.draft.identity != state.view.draft.identity || update.edit_sent)
            {
                state.editing = None;
                window.set_editing_message(false);
                state.draft_seen = state.view.draft.text.clone();
                window.set_composer_text(state.draft_seen.clone().into());
            }
            if update.error.is_none() && state.view.available {
                if let Some((position, place)) = state.pending_navigation.take() {
                    state.navigation.commit(position);
                    state.screen = place.screen;
                    window.set_active_view(place.screen);
                } else if old.conversation.as_ref().map(|c| &c.chat.id)
                    != state.view.conversation.as_ref().map(|c| &c.chat.id)
                    || !old.available
                {
                    let place = Place {
                        screen: state.screen,
                        chat: state.view.conversation.as_ref().map(|c| c.chat.id.clone()),
                    };
                    state.navigation.record(place);
                }
            } else if update.error.is_some() {
                state.pending_navigation = None;
            }
            let empty = View::default();
            project(window, &state, if original_changed { &empty } else { &old });
            documents_ui::project(window, &state, &old);
            window.set_session_loading(false);
            window.set_chat_operation("".into());
            if let Some(error) = update.error {
                state.last_error = Some(error);
            } else if update.command_response {
                state.last_error = None;
            }
        }
        project_actions(window, &self.state.borrow());
        settings_ui::project(window, &mut self.state.borrow_mut());
        profile_ui::project(window, &self.state.borrow());
        agents_ui::project(window, &self.state.borrow());
        extensions_ui::project(window, &self.state.borrow());
        catalog_ui::project(window, &self.state.borrow());
        {
            let mut state = self.state.borrow_mut();
            let g = window.global::<NativeTranscription>();
            let identity = serde_json::json!([
                state
                    .transcription_panel
                    .review
                    .as_ref()
                    .map(|r| &r.summary["review_id"]),
                g.get_executable().as_str(),
                g.get_model().as_str(),
                g.get_normalize()
            ])
            .to_string();
            if identity != state.transcription_identity {
                state.transcription_identity = identity;
                g.set_confirmed(false);
            }
        }
        transcription_ui::project(window, &self.state.borrow());
        memory_ui::project(window, &self.state.borrow());
        historical_ui::project(window, &self.state.borrow());
        mlx_ui::project(window, &mut self.state.borrow_mut());
        tasks_ui::project(window, &mut self.state.borrow_mut());
        if let Some(error) = &self.state.borrow().last_error {
            window.set_status_text(error.clone().into());
        } else if let Some(notice) = &self.state.borrow().export_notice {
            window.set_status_text(notice.clone().into());
        }
    }
    /// Flush the last visible edit before closing. Shutdown never reports a
    /// clean draft save when CAS or transport fails.
    pub fn finish(&self, window: &MainWindow) -> crate::bridge::Result<()> {
        {
            let mut state = self.state.borrow_mut();
            capture_draft(window, &self.tx, &mut state);
        }
        let (reply, wait) = mpsc::channel();
        self.tx
            .send((self.state.borrow().epoch.current(), Command::Flush(reply)))
            .map_err(|_| "Native worker stopped before saving its draft.")?;
        wait.recv_timeout(Duration::from_secs(30))
            .map_err(|_| "Native draft save has not finished; clean save is unconfirmed.")?
    }
    pub fn ready(&self) -> bool {
        self.state.borrow().view.available
    }
    pub fn view(&self) -> View {
        self.state.borrow().view.clone()
    }
}
fn strings(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}
fn capture_draft(window: &MainWindow, tx: &mpsc::Sender<(u64, Command)>, state: &mut UiState) {
    if !state.view.available || window.get_session_loading() || state.editing.is_some() {
        return;
    }
    let text = window.get_composer_text().to_string();
    if text != state.draft_seen {
        state.draft_seen = text.clone();
        state.view.draft.text = text.clone();
        state.view.draft.dirty = true;
        let _ = tx.send((
            state.epoch.current(),
            Command::DraftChanged(state.view.draft.identity, text),
        ));
    }
}
fn before_navigation(
    weak: &slint::Weak<MainWindow>,
    tx: &mpsc::Sender<(u64, Command)>,
    state: &mut UiState,
) -> bool {
    let Some(window) = weak.upgrade() else {
        return false;
    };
    if window.get_session_loading() {
        return false;
    }
    if state.editing.is_some() {
        window.set_status_text("Send or cancel the message edit before navigating.".into());
        return false;
    }
    capture_draft(&window, tx, state);
    message_cards_ui::invalidate(state);
    project(&window, state, &View::default());
    true
}
fn wire_management(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.on_edit_message(move |_, id| {
            let mut s = state.borrow_mut();
            let Some(w) = weak.upgrade() else {
                return;
            };
            if w.get_streaming() || w.get_session_loading() {
                return;
            }
            capture_draft(&w, &tx, &mut s);
            let message = s
                .view
                .conversation
                .as_ref()
                .and_then(|c| {
                    c.active_messages
                        .iter()
                        .find(|m| m.id == id.as_str() && m.role == "user")
                })
                .cloned();
            if let (Some(message), Some(intent)) = (message, s.intent.clone()) {
                s.editing = Some((intent, message.id));
                w.set_editing_message(true);
                w.set_composer_text(message.content.into());
            }
        });
    }
    {
        let state = state.clone();
        let weak = window.as_weak();
        window.on_native_cancel_edit(move || {
            let mut s = state.borrow_mut();
            if let Some(w) = weak.upgrade() {
                s.editing = None;
                s.draft_seen = s.view.draft.text.clone();
                w.set_editing_message(false);
                w.set_composer_text(s.draft_seen.clone().into());
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.on_rename_session(move |index, title| {
            let mut s = state.borrow_mut();
            if !before_navigation(&weak, &tx, &mut s) || s.trash {
                return;
            }
            let chat = usize::try_from(index)
                .ok()
                .and_then(|i| filtered_chats(&s).get(i).copied())
                .cloned();
            if let Some(chat) = chat {
                let _ = tx.send((
                    s.epoch.current(),
                    Command::Rename(chat, s.view.config.clone(), title.to_string()),
                ));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.on_delete_session(move |index| {
            let mut s = state.borrow_mut();
            if !before_navigation(&weak, &tx, &mut s) {
                return;
            }
            let chat = usize::try_from(index)
                .ok()
                .and_then(|i| filtered_chats(&s).get(i).copied())
                .cloned();
            if let Some(chat) = chat {
                let _ = tx.send((
                    s.epoch.current(),
                    Command::Trash(vec![chat], s.view.actions.clone(), s.trash),
                ));
            }
        });
    }
    for redo in [false, true] {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        let callback = move || {
            let mut s = state.borrow_mut();
            if !before_navigation(&weak, &tx, &mut s) {
                return;
            }
            if s.view.draft.dirty || s.view.draft.conflict {
                if let Some(w) = weak.upgrade() {
                    w.set_status_text(
                        "Wait for the draft to save before changing action history.".into(),
                    );
                }
                return;
            }
            let action = if redo {
                &s.view.actions.redo
            } else {
                &s.view.actions.undo
            };
            if action.is_none() {
                return;
            }
            let _ = tx.send((
                s.epoch.current(),
                Command::Reverse(s.view.actions.clone(), redo),
            ));
        };
        if redo {
            window.on_native_redo(callback);
        } else {
            window.on_native_undo(callback);
        }
    }
    {
        let state = state.clone();
        let weak = window.as_weak();
        let tx = tx.clone();
        window.on_native_toggle_trash(move || {
            let mut s = state.borrow_mut();
            if !before_navigation(&weak, &tx, &mut s) {
                return;
            }
            s.trash = !s.trash;
            s.batch.clear();
            s.batch.restore = s.trash;
            if let Some(w) = weak.upgrade() {
                w.set_native_trash_view(s.trash);
                w.set_renaming_session(-1);
                w.set_confirming_delete_session(-1);
                project_sessions(&w, &s);
            }
            let _ = tx.send((s.epoch.current(), Command::Refresh));
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_native_discard_draft(move || {
            let s = state.borrow();
            let _ = tx.send((s.epoch.current(), Command::DiscardDraft));
        });
    }
    for direction in [-1, 1] {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        let callback = move || {
            let mut s = state.borrow_mut();
            if !before_navigation(&weak, &tx, &mut s) {
                return;
            }
            let Some((index, place)) = s.navigation.target(direction) else {
                return;
            };
            s.pending_navigation = Some((index, place.clone()));
            let epoch = s.epoch.advance();
            if let Some(w) = weak.upgrade() {
                w.set_session_loading(true);
            }
            let command = match place.chat {
                Some(id) => Command::Open(id),
                None => Command::Home,
            };
            let _ = tx.send((epoch, command));
        };
        if direction > 0 {
            window.on_native_forward(callback);
        } else {
            window.on_native_back(callback);
        }
    }
}
fn project_actions(window: &MainWindow, state: &UiState) {
    let batch = window.global::<NativeBatch>();
    batch.set_enabled(state.view.available);
    batch.set_busy(state.batch_pending);
    batch.set_mode(state.batch_mode);
    batch.set_restore(state.trash);
    batch.set_count(state.batch.count() as i32);
    let enabled = state.view.available
        && !state.view.draft.dirty
        && !state.view.draft.conflict
        && state.editing.is_none();
    window.set_native_can_back(state.navigation.target(-1).is_some());
    window.set_native_can_forward(state.navigation.target(1).is_some());
    window.set_native_can_undo(enabled && state.view.actions.undo.is_some());
    window.set_native_can_redo(enabled && state.view.actions.redo.is_some());
    window.set_native_undo_label(
        state
            .view
            .actions
            .undo
            .as_ref()
            .map(|a| format!("Undo {}", a.label))
            .unwrap_or_else(|| "Undo".into())
            .into(),
    );
    window.set_native_redo_label(
        state
            .view
            .actions
            .redo
            .as_ref()
            .map(|a| format!("Redo {}", a.label))
            .unwrap_or_else(|| "Redo".into())
            .into(),
    );
    window.set_native_draft_conflict(state.view.draft.conflict);
    window.set_native_draft_status(
        if state.view.draft.conflict {
            "Draft not saved. Your text is retained."
        } else if state.view.draft.dirty {
            "Saving draft…"
        } else {
            ""
        }
        .into(),
    );
}
fn wire_batch(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    {
        let state = state.clone();
        let weak = window.as_weak();
        window.global::<NativeBatch>().on_toggle_mode(move || {
            let mut s = state.borrow_mut();
            if s.batch_pending {
                return;
            }
            s.batch_mode = !s.batch_mode;
            s.batch.clear();
            s.batch.restore = s.trash;
            if let Some(w) = weak.upgrade() {
                project_sessions(&w, &s);
            }
        });
    }
    {
        let state = state.clone();
        let weak = window.as_weak();
        window.global::<NativeBatch>().on_toggle(move |id| {
            let mut s = state.borrow_mut();
            if !s.batch_mode || s.batch_pending {
                return;
            }
            let row = filtered_chats(&s)
                .into_iter()
                .find(|c| c.id == id.as_str())
                .cloned();
            if let Some(row) = row {
                let history = s.view.actions.clone();
                if let Err(e) = s.batch.toggle(&row, &history) {
                    s.last_error = Some(e);
                }
                if let Some(w) = weak.upgrade() {
                    project_sessions(&w, &s);
                }
            }
        });
    }
    for select in [true, false] {
        let state = state.clone();
        let weak = window.as_weak();
        let callback = move || {
            let mut s = state.borrow_mut();
            if s.batch_pending {
                return;
            }
            if select {
                let rows = filtered_chats(&s).into_iter().cloned().collect::<Vec<_>>();
                let history = s.view.actions.clone();
                if let Err(e) = s.batch.select_visible(&rows, &history) {
                    s.last_error = Some(e);
                }
            } else {
                s.batch.clear();
            }
            if let Some(w) = weak.upgrade() {
                project_sessions(&w, &s);
            }
        };
        if select {
            window.global::<NativeBatch>().on_select_visible(callback)
        } else {
            window.global::<NativeBatch>().on_clear(callback)
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = window.as_weak();
        window.global::<NativeBatch>().on_apply(move || {
            let mut s = state.borrow_mut();
            if s.batch_pending || !s.view.available || !before_navigation(&weak, &tx, &mut s) {
                return;
            }
            let review = s
                .batch_context
                .as_ref()
                .ok_or("Workspace unavailable.".into())
                .and_then(|c| c.review(&s.batch));
            match review {
                Ok(review) => {
                    s.batch_pending = true;
                    let _ = tx.send((s.epoch.current(), Command::Batch(review)));
                }
                Err(e) => s.last_error = Some(e),
            }
        });
    }
}
fn filtered_chats(state: &UiState) -> Vec<&crate::bridge::Chat> {
    let query = state.session_query.to_lowercase();
    (if state.trash {
        &state.view.trash
    } else {
        &state.view.chats
    })
    .iter()
    .filter(|c| {
        c.title.to_lowercase().contains(&query) || c.preview.to_lowercase().contains(&query)
    })
    .collect()
}
fn project_sessions(window: &MainWindow, state: &UiState) {
    let rows = filtered_chats(state);
    let selected = state.view.conversation.as_ref().map(|c| c.chat.id.as_str());
    window.set_selected_session(
        rows.iter()
            .position(|c| Some(c.id.as_str()) == selected)
            .map(|i| i as i32)
            .unwrap_or(-1),
    );
    window.set_sessions(ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|c| SessionItem {
                id: c.id.clone().into(),
                title: c
                    .title
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .into(),
                subtitle: c
                    .preview
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .into(),
                selected: state.batch.contains(&c.id),
            })
            .collect::<Vec<_>>(),
    )));
}
fn project_models(window: &MainWindow, state: &UiState) {
    let rows: Vec<_> = state
        .view
        .models
        .iter()
        .map(|id| ModelItem {
            id: id.clone().into(),
            name: if id == "smith-demo" {
                "Demo".into()
            } else {
                id.clone().into()
            },
            provider: state.view.config.provider.clone().into(),
            host_hint: "Provider inventory · residency unknown".into(),
            loaded: false,
            downloaded: false,
            ..Default::default()
        })
        .collect();
    window.set_all_model_rows(ModelRc::new(VecModel::from(rows.clone())));
    window.set_model_rows(ModelRc::new(VecModel::from(
        rows.into_iter()
            .filter(|m| {
                m.name
                    .to_lowercase()
                    .contains(&state.model_query.to_lowercase())
            })
            .collect::<Vec<_>>(),
    )));
    window.set_models(strings(state.view.models.clone()));
    window.set_active_model(if state.view.config.model == "smith-demo" {
        "Demo".into()
    } else {
        state.view.config.model.clone().into()
    });
    window.set_default_model(state.view.config.model.clone().into());
}
fn task_item(value: &serde_json::Value) -> ActivityItem {
    let string = |key: &str| value[key].as_str().unwrap_or("").to_owned();
    let state = value["state"]
        .as_str()
        .or_else(|| value["state"]["kind"].as_str())
        .unwrap_or("unknown");
    let output = value["state"]["answer"]
        .as_str()
        .or_else(|| value["last_result"].as_str())
        .unwrap_or("");
    ActivityItem {
        id: string("id").into(),
        agent_name: "Task".into(),
        trigger: "Manual".into(),
        status: state.into(),
        input: string("input").into(),
        output: output.into(),
        error: value["state"]["reason"].as_str().unwrap_or("").into(),
        cancellable: matches!(
            state,
            "pending"
                | "running"
                | "model_thinking"
                | "needs_approval"
                | "needs_clarification"
                | "interrupted"
        ),
        needs_clarification: state == "needs_clarification",
        ..Default::default()
    }
}
fn project(window: &MainWindow, state: &UiState, old: &View) {
    window.set_native_original_pending(state.original_pending);
    window.global::<Palette>().set_dark(false);
    window.set_appearance("Light".into());
    if old.chats != state.view.chats
        || old.trash != state.view.trash
        || old.conversation.as_ref().map(|c| &c.chat.id)
            != state.view.conversation.as_ref().map(|c| &c.chat.id)
    {
        project_sessions(window, state);
    }
    if old.models != state.view.models || old.config != state.view.config {
        project_models(window, state);
    }
    if old.conversation != state.view.conversation {
        let rows = state
            .view
            .conversation
            .as_ref()
            .map(|c| c.active_messages.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(|m| {
                let text = if m.status == "streaming" && m.content.is_empty() {
                    "Processing…".to_owned()
                } else {
                    m.content.clone()
                };
                let presentation =
                    crate::messages::presentation(&m.role, &text, m.reasoning.as_deref());
                let reasoning = presentation.reasoning.to_owned();
                let reasoning_derived = presentation.derived;
                let mut segments = crate::messages::segments(presentation.answer)
                    .into_iter()
                    .map(|segment| match segment {
                        crate::messages::Segment::Markdown(text) => MessageSegment {
                            kind: "markdown".into(),
                            text: text.clone().into(),
                            styled: slint::StyledText::from_markdown(&text)
                                .unwrap_or_else(|_| slint::StyledText::from_plain_text(&text)),
                            ..Default::default()
                        },
                        crate::messages::Segment::Heading(level, text) => MessageSegment {
                            kind: format!("h{level}").into(),
                            text: text.into(),
                            ..Default::default()
                        },
                        crate::messages::Segment::Code { language, text } => MessageSegment {
                            kind: "code".into(),
                            lang: language.into(),
                            text: text.into(),
                            ..Default::default()
                        },
                    })
                    .collect::<Vec<_>>();
                segments.extend(message_cards_ui::segments(state, &m.id));
                ChatMessage {
                    historical_available: message_cards_ui::eligible(m),
                    id: m.id.clone().into(),
                    role: m.role.clone().into(),
                    text: text.into(),
                    segments: ModelRc::new(VecModel::from(segments)),
                    reasoning: reasoning.into(),
                    reasoning_derived,
                    feedback: m.feedback.clone().into(),
                    stats: format!(
                        "{}{}{}",
                        m.status,
                        if m.persisted { "" } else { " · Not saved" },
                        m.error
                            .as_ref()
                            .map(|e| format!(" · {e}"))
                            .unwrap_or_default()
                    )
                    .into(),
                    branch_index: m.branch_index as i32,
                    branch_count: m.siblings.len() as i32,
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>();
        window.set_messages(ModelRc::new(VecModel::from(rows)));
    }
    window.set_streaming(
        state
            .view
            .conversation
            .as_ref()
            .is_some_and(|c| c.active_messages.iter().any(|m| m.status == "streaming")),
    );
    if old.artifacts != state.view.artifacts {
        window.set_artifacts_list(ModelRc::new(VecModel::from(
            state
                .view
                .artifacts
                .iter()
                .map(|v| ArtifactItem {
                    id: v["id"].as_str().unwrap_or("").into(),
                    title: v["name"].as_str().unwrap_or("").into(),
                    kind: v["kind"].as_str().unwrap_or("").into(),
                    subtitle: "Local document · preview and original export".into(),
                    text_viewable: true,
                    is_image: false,
                })
                .collect::<Vec<_>>(),
        )));
    }
    if old.tasks != state.view.tasks {
        window.set_activity_tasks(ModelRc::new(VecModel::from(
            state.view.tasks.iter().map(task_item).collect::<Vec<_>>(),
        )));
    }
    window.set_server_status(if state.view.available {
        "online".into()
    } else {
        "offline".into()
    });
    window.set_status_text(state.view.status.clone().into());
}

pub fn register_reviewed_fonts() -> crate::bridge::Result<()> {
    let mut collection = slint::fontique_010::shared_collection();
    for bytes in [
        include_bytes!(concat!(
            env!("SMITH_REVIEWED_UI_DIR"),
            "/assets/fonts/Inter.ttf"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("SMITH_REVIEWED_UI_DIR"),
            "/assets/fonts/SplineSansMono.ttf"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("SMITH_REVIEWED_UI_DIR"),
            "/assets/fonts/Poppins-Regular.ttf"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("SMITH_REVIEWED_UI_DIR"),
            "/assets/fonts/Poppins-Medium.ttf"
        ))
        .as_slice(),
        include_bytes!(concat!(
            env!("SMITH_REVIEWED_UI_DIR"),
            "/assets/fonts/Poppins-SemiBold.ttf"
        ))
        .as_slice(),
    ] {
        if collection
            .register_fonts(
                slint::fontique_010::fontique::Blob::new(std::sync::Arc::new(bytes.to_vec())),
                None,
            )
            .is_empty()
        {
            return Err("Reviewed font registration failed.".into());
        }
    }
    Ok(())
}
