use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub type Result<T> = std::result::Result<T, String>;

/// Transport is supplied by an owned launch, never by a user-entered URL.
/// Implementations must bound bytes/time and must not follow redirects.
pub trait Transport: Send + 'static {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value>;
    fn catalog_mutation(
        &mut self,
        method: &str,
        path: &str,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, crate::http::CatalogMutationError> {
        self.request(method, path, Some(session), Some(body))
            .map_err(crate::http::CatalogMutationError::OutcomeUnknown)
    }
    fn transcription_start(
        &mut self,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, crate::transcription::TranscriptionSubmitError> {
        self.request(
            "POST",
            "/api/media/transcription/start",
            Some(session),
            Some(body),
        )
        .map_err(crate::transcription::TranscriptionSubmitError::OutcomeUnknown)
    }
    fn transcription_file_start(
        &mut self,
        session: &str,
        body: Value,
    ) -> std::result::Result<Value, crate::transcription::TranscriptionSubmitError> {
        self.request(
            "POST",
            "/api/media/transcription/start-file",
            Some(session),
            Some(body),
        )
        .map_err(crate::transcription::TranscriptionSubmitError::OutcomeUnknown)
    }
    /// Secret-bearing bodies require an explicitly qualified transport. Never
    /// fall back to request(), which mocks or adapters may log or retain.
    fn sensitive_request(
        &mut self,
        _method: &str,
        _path: &str,
        _session: Option<&str>,
        _body: crate::profile_bridge::SensitiveBody,
    ) -> Result<Value> {
        Err("Sensitive requests are unavailable on this transport.".into())
    }
    fn upload(&mut self, _session: &str, _metadata: Value, _bytes: &[u8]) -> Result<Value> {
        Err("Document uploads are unavailable on this transport.".into())
    }
    fn download(&mut self, _path: &str, _session: &str) -> Result<Vec<u8>> {
        Err("Document downloads are unavailable on this transport.".into())
    }
    fn event_snapshot(&mut self, _path: &str, _session: &str) -> Result<Option<Value>> {
        Ok(None)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Config {
    pub revision: String,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub tested: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub version: i64,
    #[serde(default)]
    pub preview: String,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub feedback: String,
    #[serde(default)]
    pub feedback_revision: i64,
    pub status: String,
    pub error: Option<String>,
    #[serde(default = "persisted_default")]
    pub persisted: bool,
    #[serde(default)]
    pub branch_index: usize,
    #[serde(default)]
    pub siblings: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub media: Vec<Value>,
    pub run_id: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Conversation {
    pub chat: Chat,
    /// Export/history contains every branch. Render only active_messages.
    pub messages: Vec<Message>,
    pub active_messages: Vec<Message>,
    pub active_leaf: Option<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Draft {
    pub owner: String,
    pub version: i64,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default)]
    pub media: Vec<Value>,
    #[serde(skip)]
    pub identity: u64,
    #[serde(skip)]
    pub dirty: bool,
    #[serde(skip)]
    pub conflict: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub chat_ids: Vec<String>,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Actions {
    pub revision: i64,
    pub undo: Option<Action>,
    pub redo: Option<Action>,
}
#[derive(Clone, Debug, Default)]
pub struct View {
    pub config: Config,
    pub chats: Vec<Chat>,
    pub conversation: Option<Conversation>,
    pub models: Vec<String>,
    pub artifacts: Vec<Value>,
    pub tasks: Vec<Value>,
    pub draft: Draft,
    pub actions: Actions,
    pub trash: Vec<Chat>,
    pub status: String,
    pub available: bool,
    pub synthetic: bool,
    pub settings: crate::settings::Settings,
    pub documents: crate::documents::Documents,
    pub mlx: Value,
}

/// Captured from the displayed owner and revisions *before* enqueueing work.
/// These fields never silently refresh and retry a rejected mutation.
#[derive(Clone, Debug)]
pub struct Intent {
    session: String,
    pub chat: Chat,
    pub config: Config,
}

pub struct Bridge<T: Transport> {
    pub(crate) transport: T,
    pub(crate) session: Option<String>,
    pub view: View,
    visible: std::collections::BTreeMap<String, Value>,
    draft_identity: u64,
}
impl<T: Transport> Bridge<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            session: None,
            view: View::default(),
            visible: Default::default(),
            draft_identity: 0,
        }
    }
    pub(crate) fn get(&mut self, path: &str) -> Result<Value> {
        self.transport
            .request("GET", path, self.session.as_deref(), None)
    }
    pub fn initialize(&mut self) -> Result<()> {
        let status = self
            .transport
            .request("GET", "/api/profile/status", None, None)?;
        if status["state"] != "unlocked" {
            self.revoke();
            return Err("The owned local workspace is not ready. No password or login is requested by this native bridge.".into());
        }
        let session = status["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Workspace admission is missing.")?
            .to_owned();
        self.session = Some(session);
        self.view.synthetic = status["synthetic"] == true;
        self.view.config = decode(self.get("/api/config")?)?;
        self.view.available = true;
        self.refresh_lists()?;
        self.load_draft("new")?;
        self.view.status = if self.view.synthetic {
            "Synthetic workspace · original native interface"
        } else {
            ""
        }
        .into();
        Ok(())
    }
    pub fn revoke(&mut self) {
        self.session = None;
        self.visible.clear();
        self.view = View {
            status: "Local workspace became unavailable. Its content was cleared.".into(),
            ..View::default()
        };
    }
    pub fn check_session(&mut self) -> Result<()> {
        let status = self
            .transport
            .request("GET", "/api/profile/status", None, None)?;
        if status["state"] != "unlocked" || status["session"].as_str() != self.session.as_deref() {
            self.revoke();
            return Err(
                "Workspace ownership changed. Pending native actions were discarded.".into(),
            );
        }
        Ok(())
    }
    pub fn refresh_lists(&mut self) -> Result<()> {
        self.view.chats = decode(self.get("/api/chats")?)?;
        if self.view.config.provider != "mock" && !self.view.config.tested {
            // Restart never retests or rediscovers a saved external destination.
            // Local history/documents/tasks remain available independently.
            self.view.models.clear();
        } else {
            let revision = encode_component(&self.view.config.revision);
            let models = self.get(&format!("/api/models?revision={revision}"))?;
            if models["revision"] != self.view.config.revision {
                return Err("Model result belongs to a different connection revision.".into());
            }
            self.view.models = models["models"]
                .as_array()
                .ok_or("Malformed model list.")?
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect();
        }
        let artifacts = self.get("/api/media/artifacts")?;
        self.view.artifacts = artifacts
            .as_array()
            .or_else(|| artifacts["artifacts"].as_array())
            .ok_or("Malformed artifact list.")?
            .clone();
        let tasks = self.get("/api/agent/tasks")?;
        self.view.tasks = tasks
            .as_array()
            .or_else(|| tasks["tasks"].as_array())
            .ok_or("Malformed task list.")?
            .clone();
        self.refresh_actions()?;
        Ok(())
    }
    pub fn open(&mut self, id: &str) -> Result<()> {
        self.save_draft()?;
        let previous = self.view.conversation.clone();
        self.refresh_conversation(id)?;
        if let Err(error) = self.load_draft(id) {
            self.view.conversation = previous;
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn refresh_conversation(&mut self, id: &str) -> Result<()> {
        self.check_session()?;
        let mut conversation: Conversation =
            decode(self.get(&format!("/api/chats/{}", encode_component(id)))?)?;
        if conversation.chat.id != id {
            return Err("Conversation identity did not match the request.".into());
        }
        for message in &mut conversation.active_messages {
            if let Some(value) = self.visible.get(&message.id) {
                overlay(message, value);
            }
        }
        self.view.conversation = Some(conversation);
        Ok(())
    }
    pub fn new_chat(&mut self) -> Result<()> {
        self.save_draft()?;
        self.check_session()?;
        let body = expected(&self.view.config);
        let chat: Chat = decode(self.transport.request(
            "POST",
            "/api/chats",
            self.session.as_deref(),
            Some(body),
        )?)?;
        self.open(&chat.id)?;
        self.refresh_lists()
    }
    pub fn home(&mut self) -> Result<()> {
        self.save_draft()?;
        self.load_draft("new")?;
        self.view.conversation = None;
        Ok(())
    }
    pub fn select_model(&mut self, reviewed: &Config, model: &str) -> Result<()> {
        if reviewed != &self.view.config || !self.view.models.iter().any(|id| id == model) {
            return Err("Choose a model from the displayed connection before retrying.".into());
        }
        let mut body = expected(reviewed);
        body["model"] = json!(model);
        let selected: Config = decode(self.transport.request(
            "PUT",
            "/api/model",
            self.session.as_deref(),
            Some(body),
        )?)?;
        if selected.provider != reviewed.provider
            || selected.endpoint != reviewed.endpoint
            || selected.model != model
        {
            return Err("Model selection returned a different destination.".into());
        }
        self.view.config = selected;
        self.refresh_lists()
    }
    pub fn intent(&self) -> Result<Intent> {
        Ok(Intent {
            session: self.session.clone().ok_or("Workspace unavailable.")?,
            chat: self
                .view
                .conversation
                .as_ref()
                .ok_or("Choose a conversation first.")?
                .chat
                .clone(),
            config: self.view.config.clone(),
        })
    }
    fn validate(&self, intent: &Intent) -> Result<()> {
        if self.session.as_deref() != Some(&intent.session) {
            return Err("Stale workspace action.".into());
        }
        if intent.config != self.view.config {
            return Err("Stale connection action. Review the connection before retrying.".into());
        }
        Ok(())
    }
    pub fn send(&mut self, intent: &Intent, text: &str) -> Result<()> {
        self.validate(intent)?;
        self.save_draft()?;
        if text.trim().is_empty() || text.len() > 100_000 {
            return Err("Enter a message within the text limit.".into());
        }
        let mut body = expected(&intent.config);
        body["version"] = json!(intent.chat.version);
        body["model"] = json!(intent.config.model);
        body["content"] = json!(text);
        let draft = &self.view.draft;
        if draft.owner != intent.chat.id {
            return Err("The composer belongs to a different conversation.".into());
        }
        body["attachments"] = json!(draft.attachments);
        body["media"] = json!(draft.media);
        let result = self.transport.request(
            "POST",
            &format!("/api/chats/{}/messages", encode_component(&intent.chat.id)),
            Some(&intent.session),
            Some(body),
        )?;
        let conversation: Conversation = decode(result["conversation"].clone())?;
        if conversation.chat.id != intent.chat.id {
            return Err("Response belongs to another conversation.".into());
        }
        self.view.conversation = Some(conversation);
        self.view.status = if intent.config.provider == "managed" {
            "Processing with the owned local model. Stop unloads that model."
        } else {
            ""
        }
        .into();
        if self.view.draft.text == text {
            self.view.draft.text.clear();
            self.view.draft.attachments.clear();
            self.view.draft.media.clear();
            self.view.draft.dirty = true;
            // The prompt is accepted even if clearing its saved draft fails.
            // Preserve the failed CAS state for explicit recovery, never replay.
            if let Err(error) = self.save_draft() {
                self.view.status = error;
            }
        }
        self.refresh_lists()
    }
    pub fn regenerate(&mut self, intent: &Intent, message: &str) -> Result<()> {
        self.validate(intent)?;
        let mut body = expected(&intent.config);
        body["version"] = json!(intent.chat.version);
        body["message_id"] = json!(message);
        body["model"] = json!(intent.config.model);
        let response = self.transport.request(
            "POST",
            &format!(
                "/api/chats/{}/regenerate",
                encode_component(&intent.chat.id)
            ),
            Some(&intent.session),
            Some(body),
        )?;
        self.view.conversation = Some(decode(response["conversation"].clone())?);
        self.view.status = if intent.config.provider == "managed" {
            "Processing with the owned local model. Stop unloads that model."
        } else {
            ""
        }
        .into();
        self.refresh_actions()
    }
    pub fn branch(&mut self, intent: &Intent, message: &str) -> Result<()> {
        self.validate(intent)?;
        let mut body = expected(&intent.config);
        body["version"] = json!(intent.chat.version);
        body["message_id"] = json!(message);
        let response = self.transport.request(
            "POST",
            &format!("/api/chats/{}/branch", encode_component(&intent.chat.id)),
            Some(&intent.session),
            Some(body),
        )?;
        self.view.conversation = Some(decode(response)?);
        self.refresh_actions()
    }
    pub fn cancel(&mut self) -> Result<()> {
        let run = self
            .view
            .conversation
            .as_ref()
            .and_then(|c| {
                c.active_messages
                    .iter()
                    .rev()
                    .find(|m| m.status == "streaming")
            })
            .and_then(|m| m.run_id.as_deref())
            .ok_or("No active response.")?
            .to_owned();
        self.transport.request(
            "POST",
            &format!("/api/runs/{}/cancel", encode_component(&run)),
            self.session.as_deref(),
            Some(json!({})),
        )?;
        // Acceptance is not proof of cancellation; poll until terminal.
        self.view.status = "Stopping response…".into();
        self.poll()
    }
    pub fn poll(&mut self) -> Result<()> {
        if let Err(error) = self.check_session() {
            self.revoke();
            return Err(error);
        }
        if !self.view.mlx.is_null() {
            self.refresh_mlx()?;
        }
        if !self.view.settings.setup.is_null() {
            self.refresh_settings()?;
        }
        if self.view.draft.dirty && !self.view.draft.conflict {
            self.save_draft()?;
        }
        let run = self
            .view
            .conversation
            .as_ref()
            .and_then(|c| {
                c.active_messages
                    .iter()
                    .rev()
                    .find(|m| m.status == "streaming")
            })
            .and_then(|m| m.run_id.clone());
        if let (Some(run), Some(session)) = (run, self.session.clone()) {
            if let Some(state) = self.transport.event_snapshot(
                &format!("/api/runs/{}/events", encode_component(&run)),
                &session,
            )? {
                let id = state["message_id"]
                    .as_str()
                    .ok_or("Answer snapshot lacks its owner.")?
                    .to_owned();
                let expected = self.view.conversation.as_ref().is_some_and(|c| {
                    c.active_messages
                        .iter()
                        .any(|m| m.id == id && m.run_id.as_deref() == Some(&run))
                });
                if !expected {
                    return Err("Answer snapshot belongs to another run.".into());
                }
                if state["persisted"] == false {
                    self.visible.insert(id.clone(), state.clone());
                } else {
                    self.visible.remove(&id);
                }
                if let Some(c) = &mut self.view.conversation {
                    for message in &mut c.active_messages {
                        if message.id == id {
                            overlay(message, &state);
                        }
                    }
                }
                if self.streaming() {
                    return Ok(());
                }
            }
        }
        if let Some(id) = self.view.conversation.as_ref().map(|c| c.chat.id.clone()) {
            self.refresh_conversation(&id)?;
            if !self.streaming() {
                self.view.status = if self.view.synthetic {
                    "Synthetic response finished · no model was loaded"
                } else {
                    ""
                }
                .into();
            }
        }
        Ok(())
    }
    pub fn streaming(&self) -> bool {
        self.view
            .conversation
            .as_ref()
            .is_some_and(|c| c.active_messages.iter().any(|m| m.status == "streaming"))
    }
    pub fn refresh_actions(&mut self) -> Result<()> {
        self.view.actions = decode(self.get("/api/actions")?)?;
        let trash = self.get("/api/trash")?;
        self.view.trash = decode(trash["chats"].clone())?;
        Ok(())
    }
    fn load_draft(&mut self, owner: &str) -> Result<()> {
        let mut draft: Draft =
            decode(self.get(&format!("/api/drafts/{}", encode_component(owner)))?)?;
        if draft.owner != owner {
            return Err("Draft identity did not match its conversation.".into());
        }
        self.draft_identity = self
            .draft_identity
            .checked_add(1)
            .ok_or("Draft identity exhausted.")?;
        draft.identity = self.draft_identity;
        self.view.draft = draft;
        Ok(())
    }
    /// Local edits use the loaded owner identity, not a guessed new server
    /// version. A single worker serializes CAS saves; navigation flushes first.
    pub fn change_draft(&mut self, identity: u64, text: String) -> Result<()> {
        if !self.view.available || identity != self.view.draft.identity {
            return Err("Discarded a late edit from another composer.".into());
        }
        if text.len() > 100_000 {
            return Err("Draft text is limited to 100,000 bytes.".into());
        }
        if self.view.draft.text != text {
            self.view.draft.text = text;
            self.view.draft.dirty = true;
        }
        Ok(())
    }
    pub fn save_draft(&mut self) -> Result<()> {
        if !self.view.draft.dirty {
            return Ok(());
        }
        if self.view.draft.conflict {
            return Err("Your unsaved draft is retained. Resolve the saved-draft conflict before leaving this conversation.".into());
        }
        let previous = self.view.draft.clone();
        let result = self.transport.request("PUT", &format!("/api/drafts/{}", encode_component(&previous.owner)), self.session.as_deref(), Some(json!({"version":previous.version,"text":previous.text,"attachments":previous.attachments,"media":previous.media})));
        let mut saved: Draft = match result.and_then(decode) {
            Ok(value) => value,
            Err(error) => {
                self.view.draft.conflict = true;
                return Err(format!(
                    "Draft not saved; visible text is retained. {error}"
                ));
            }
        };
        if saved.owner != previous.owner || saved.version <= previous.version {
            self.view.draft.conflict = true;
            return Err(
                "Draft response did not match its owner/version. Visible text is retained.".into(),
            );
        }
        saved.identity = previous.identity;
        self.view.draft = saved;
        self.refresh_actions()
    }
    /// Only an explicit discard/reload control may call this after a conflict.
    pub fn discard_and_reload_draft(&mut self) -> Result<()> {
        let owner = self.view.draft.owner.clone();
        self.load_draft(&owner)
    }
    pub fn rename(&mut self, chat: &Chat, config: &Config, title: &str) -> Result<()> {
        if config != &self.view.config {
            return Err("Connection changed; rename was not replayed.".into());
        }
        self.save_draft()?;
        let mut body = expected(config);
        body["version"] = json!(chat.version);
        body["title"] = json!(title);
        self.transport.request(
            "PATCH",
            &format!("/api/chats/{}", encode_component(&chat.id)),
            self.session.as_deref(),
            Some(body),
        )?;
        if self
            .view
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id == chat.id)
        {
            self.refresh_conversation(&chat.id)?;
        }
        self.refresh_lists()
    }
    pub fn trash(&mut self, targets: &[Chat], history: &Actions, restore: bool) -> Result<()> {
        self.save_draft()?;
        self.transport.request("POST", if restore {"/api/chats/restore"} else {"/api/chats/trash"}, self.session.as_deref(), Some(json!({"chats":targets.iter().map(|c|json!({"id":c.id,"version":c.version})).collect::<Vec<_>>(),"history_revision":history.revision})))?;
        self.refresh_lists()?;
        self.reconcile_selected()
    }
    pub fn reverse(&mut self, history: &Actions, redo: bool) -> Result<()> {
        self.save_draft()?;
        let action = if redo { &history.redo } else { &history.undo }
            .as_ref()
            .ok_or("There is no reversible action.")?;
        self.transport.request(
            "POST",
            if redo {
                "/api/actions/redo"
            } else {
                "/api/actions/undo"
            },
            self.session.as_deref(),
            Some(json!({"revision":history.revision,"action_id":action.id})),
        )?;
        self.refresh_lists()?;
        self.reconcile_selected()
    }
    fn reconcile_selected(&mut self) -> Result<()> {
        if let Some(id) = self.view.conversation.as_ref().map(|c| c.chat.id.clone()) {
            if self.view.chats.iter().any(|c| c.id == id) {
                self.refresh_conversation(&id)?;
            } else {
                self.view.conversation = None;
                self.load_draft("new")?;
            }
        }
        Ok(())
    }
    pub fn edit(&mut self, intent: &Intent, message: &str, text: &str) -> Result<()> {
        self.validate(intent)?;
        self.save_draft()?;
        let mut body = expected(&intent.config);
        body["message_id"] = json!(message);
        body["version"] = json!(intent.chat.version);
        body["model"] = json!(intent.config.model);
        body["content"] = json!(text);
        // Omitted attachment/media fields preserve the original immutable inputs.
        let response = self.transport.request(
            "POST",
            &format!("/api/chats/{}/edit", encode_component(&intent.chat.id)),
            Some(&intent.session),
            Some(body),
        )?;
        let conversation: Conversation = decode(response["conversation"].clone())?;
        if conversation.chat.id != intent.chat.id {
            return Err("Edited response belongs to another conversation.".into());
        }
        self.view.conversation = Some(conversation);
        self.refresh_lists()
    }
}

fn overlay(message: &mut Message, state: &Value) {
    if let Some(content) = state["content"].as_str() {
        message.content = content.to_owned();
    }
    if let Some(status) = state["status"].as_str() {
        message.status = status.to_owned();
    }
    message.error = state["error"].as_str().map(str::to_owned);
    message.persisted = state["persisted"].as_bool().unwrap_or(true);
    // IDs, branches and attachment metadata always come from fresh history.
}
fn persisted_default() -> bool {
    true
}

fn decode<T: for<'a> Deserialize<'a>>(value: Value) -> Result<T> {
    serde_json::from_value(value)
        .map_err(|_| "Backend response did not match the native bridge contract.".into())
}
fn expected(config: &Config) -> Value {
    json!({"revision":config.revision,"provider":config.provider,"endpoint":config.endpoint})
}
pub fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// UI-local generation invalidates late results after navigation/revocation.
#[derive(Clone, Copy, Debug, Default)]
pub struct NavigationEpoch(u64);
impl NavigationEpoch {
    pub fn advance(&mut self) -> u64 {
        self.0 = self.0.checked_add(1).expect("epoch exhaustion");
        self.0
    }
    pub fn current(&self) -> u64 {
        self.0
    }
    pub fn accepts(&self, candidate: u64) -> bool {
        candidate == self.0
    }
}
