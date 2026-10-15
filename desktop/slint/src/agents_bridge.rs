//! Saved personas are preferences over an existing explicitly granted workspace.
//! Reviews bind session, definition revision, exact subset and optional local model.
//! Launch creates a Pending task only; no implicit model run or retry follows.
use crate::{
    bridge::{encode_component, Bridge, Config, Result, Transport},
    task_bridge::{
        new_task_request_id, LocalModelReview, Permission, ResourceScope, TaskSnapshot,
        WorkspaceSnapshot,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentModel {
    SelectedAtLaunch,
    Pinned {
        provider: String,
        endpoint: String,
        model: String,
        scope_id: String,
    },
}
impl AgentModel {
    pub fn label(&self) -> String {
        match self {
            Self::SelectedAtLaunch => "Selected local model at launch".into(),
            Self::Pinned { model, .. } => format!("Pinned: {model}"),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub system_prompt: String,
    pub soul: String,
    pub workspace_id: String,
    pub policy_revision: u64,
    pub capabilities: Vec<Permission>,
    pub resources: Vec<ResourceScope>,
    pub model: AgentModel,
    #[serde(
        default,
        skip_serializing_if = "crate::extensions_bridge::InstructionSelection::is_enabled"
    )]
    pub extensions: crate::extensions_bridge::InstructionSelection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub revision: u64,
    pub definition: AgentConfig,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub deleted: bool,
}
#[derive(Clone, Default, Deserialize)]
pub struct AgentCatalog {
    pub definitions: Vec<AgentDefinition>,
    #[serde(skip)]
    reviews: Vec<AgentReview>,
}
impl AgentCatalog {
    pub fn review(&self, definition: &AgentDefinition) -> Result<AgentReview> {
        self.reviews
            .iter()
            .find(|r| r.definition == *definition)
            .cloned()
            .ok_or_else(|| {
                "This definition was not fetched in this saved-agent catalog. Refresh it.".into()
            })
    }
}
#[derive(Clone)]
pub struct AgentReview {
    used: Arc<AtomicBool>,
    session: String,
    pub(crate) definition: AgentDefinition,
}
impl AgentReview {
    pub fn definition(&self) -> &AgentDefinition {
        &self.definition
    }
}
#[derive(Clone)]
pub struct AgentEditReview {
    used: Arc<AtomicBool>,
    session: String,
    existing: Option<AgentDefinition>,
    config: AgentConfig,
}
impl AgentEditReview {
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }
}
#[derive(Clone)]
pub struct AgentLaunchReview {
    agent: AgentReview,
    input: String,
    request_id: String,
    model: Option<LocalModelReview>,
}
impl AgentLaunchReview {
    pub fn input(&self) -> &str {
        &self.input
    }
    pub fn definition(&self) -> &AgentDefinition {
        self.agent.definition()
    }
    pub fn model(&self) -> Option<&LocalModelReview> {
        self.model.as_ref()
    }
}
#[derive(Clone)]
pub struct AgentEditor {
    create_used: Arc<AtomicBool>,
    session: String,
    config: Config,
}
impl AgentEditor {
    pub fn edit(
        &self,
        existing: Option<&AgentReview>,
        config: AgentConfig,
        workspace: &WorkspaceSnapshot,
    ) -> Result<AgentEditReview> {
        if existing.is_some_and(|v| v.session != self.session || v.definition.deleted) {
            return Err("This editor belongs to an earlier or deleted agent.".into());
        }
        validate(&config, workspace)?;
        Ok(AgentEditReview {
            used: existing
                .map(|v| v.used.clone())
                .unwrap_or_else(|| self.create_used.clone()),
            session: self.session.clone(),
            existing: existing.map(|v| v.definition.clone()),
            config,
        })
    }
    pub fn launch(
        &self,
        agent: &AgentReview,
        input: String,
        model: Option<&LocalModelReview>,
    ) -> Result<AgentLaunchReview> {
        let mut bridge = Bridge::new(NoTransport);
        bridge.session = Some(self.session.clone());
        bridge.view.available = true;
        bridge.view.config = self.config.clone();
        bridge.agent_launch_review(agent, input, model)
    }
}
struct NoTransport;
impl Transport for NoTransport {
    fn request(
        &mut self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        Err("No transport in captured editor.".into())
    }
}
fn consume(used: &AtomicBool) -> Result<()> {
    if used.swap(true, Ordering::SeqCst) {
        Err("This saved-agent change was already attempted. Refresh and inspect before another action.".into())
    } else {
        Ok(())
    }
}
fn bound_review(session: &str, definition: AgentDefinition) -> Result<AgentReview> {
    if definition.id.is_empty()
        || definition.id.len() > 128
        || definition.id.chars().any(char::is_control)
        || definition.revision == 0
    {
        return Err("Invalid saved-agent identity.".into());
    }
    Ok(AgentReview {
        session: session.into(),
        definition,
        used: Arc::new(AtomicBool::new(false)),
    })
}
fn validate(config: &AgentConfig, workspace: &WorkspaceSnapshot) -> Result<()> {
    config.extensions.validate()?;
    if config.name.trim().is_empty()
        || config.name.len() > 80
        || config.name.chars().any(char::is_control)
        || config.system_prompt.len() > 8192
        || config.soul.len() > 4096
    {
        return Err("Use a name of 1–80 bytes, system prompt up to 8192 bytes, and persona up to 4096 bytes.".into());
    }
    if !workspace.available
        || config.workspace_id != workspace.id
        || config.policy_revision != workspace.policy_revision
    {
        return Err("Select the current available workspace policy.".into());
    }
    let capabilities: BTreeSet<_> = config.capabilities.iter().copied().collect();
    let resources: BTreeSet<_> = config.resources.iter().cloned().collect();
    if capabilities.len() != config.capabilities.len()
        || resources.len() != config.resources.len()
        || !config
            .capabilities
            .iter()
            .all(|v| workspace.capabilities.contains(v))
        || !config
            .resources
            .iter()
            .all(|v| workspace.resources.contains(v))
    {
        return Err("Agent permissions must be an exact subset of the reviewed workspace.".into());
    }
    Ok(())
}
impl<T: Transport> Bridge<T> {
    pub fn agent_editor(&self) -> Result<AgentEditor> {
        Ok(AgentEditor {
            create_used: Arc::new(AtomicBool::new(false)),
            session: self.agent_session()?,
            config: self.view.config.clone(),
        })
    }
    fn agent_session(&self) -> Result<String> {
        if !self.view.available {
            return Err("Workspace unavailable.".into());
        }
        self.session
            .clone()
            .ok_or_else(|| "Workspace unavailable.".into())
    }
    fn agent_check(&mut self, session: &str) -> Result<()> {
        if self.session.as_deref() != Some(session) {
            return Err("This saved-agent review belongs to an earlier workspace.".into());
        }
        self.check_session()
    }
    pub fn agent_catalog(&mut self) -> Result<AgentCatalog> {
        let session = self.agent_session()?;
        let mut definitions = Vec::new();
        let mut after = None::<String>;
        let mut seen = BTreeSet::new();
        for page_index in 0..8 {
            self.agent_check(&session)?;
            let path = format!(
                "/api/agent/definitions?include_deleted=true&limit=32{}",
                after
                    .as_ref()
                    .map(|v| format!("&after={}", encode_component(v)))
                    .unwrap_or_default()
            );
            let value = self.get(&path)?;
            self.agent_check(&session)?;
            let page: AgentCatalog =
                serde_json::from_value(value.clone()).map_err(|_| "Invalid saved-agent list.")?;
            if page.definitions.len() > 32
                || definitions.len() + page.definitions.len() > 256
                || page.definitions.iter().any(|v| !seen.insert(v.id.clone()))
            {
                return Err("Saved-agent pagination exceeded its bound or repeated rows.".into());
            }
            let final_id = page.definitions.last().map(|d| d.id.clone());
            definitions.extend(page.definitions);
            match value["next_after"].as_str() {
                None => break,
                Some(next)
                    if next.len() == 36
                        && after.as_deref() != Some(next)
                        && final_id.as_deref() == Some(next)
                        && page_index < 7 =>
                {
                    after = Some(next.to_owned());
                }
                _ => return Err("Invalid saved-agent page cursor.".into()),
            }
            if definitions.len() >= 256 {
                return Err("Saved-agent list has an unexpected additional page.".into());
            }
        }
        let reviews = definitions
            .iter()
            .map(|row| bound_review(&session, row.clone()))
            .collect::<Result<Vec<_>>>()?;
        Ok(AgentCatalog {
            definitions,
            reviews,
        })
    }
    pub fn agent_restore(&mut self, review: &AgentReview) -> Result<AgentReview> {
        self.agent_check(&review.session)?;
        if !review.definition.deleted {
            return Err("Select a deleted definition to restore.".into());
        }
        consume(&review.used)?;
        let result = self.transport.request(
            "POST",
            &format!(
                "/api/agent/definitions/{}/restore",
                encode_component(&review.definition.id)
            ),
            Some(&review.session),
            Some(json!({"expected_revision":review.definition.revision})),
        );
        self.agent_check(&review.session)?;
        let saved: AgentDefinition = serde_json::from_value(result?["definition"].clone())
            .map_err(|_| "Invalid restored-agent receipt.")?;
        if saved.id != review.definition.id
            || saved.deleted
            || saved.definition != review.definition.definition
            || saved.created_at != review.definition.created_at
            || saved.revision != review.definition.revision + 1
        {
            return Err("Restore receipt differs from the reviewed definition.".into());
        }
        bound_review(&review.session, saved)
    }
    pub fn agent_edit_review(
        &self,
        existing: Option<&AgentReview>,
        config: AgentConfig,
        workspace: &WorkspaceSnapshot,
    ) -> Result<AgentEditReview> {
        let session = self.agent_session()?;
        if existing.is_some_and(|r| r.session != session) {
            return Err("Stale saved-agent editor.".into());
        }
        validate(&config, workspace)?;
        Ok(AgentEditReview {
            used: existing
                .map(|v| v.used.clone())
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            session,
            existing: existing.map(|r| r.definition.clone()),
            config,
        })
    }
    pub fn agent_save(&mut self, review: &AgentEditReview, confirmed: bool) -> Result<AgentReview> {
        if !confirmed {
            return Err("Confirm the displayed persona and permissions before saving.".into());
        }
        self.agent_check(&review.session)?;
        let (method, path, body) = if let Some(old) = &review.existing {
            (
                "PUT",
                format!("/api/agent/definitions/{}", encode_component(&old.id)),
                json!({"expected_revision":old.revision,"definition":review.config,"confirmed":true}),
            )
        } else {
            (
                "POST",
                "/api/agent/definitions".into(),
                json!({"definition":review.config,"confirmed":true}),
            )
        };
        consume(&review.used)?;
        let result = self
            .transport
            .request(method, &path, Some(&review.session), Some(body));
        self.agent_check(&review.session)?;
        let saved: AgentDefinition = serde_json::from_value(result?["definition"].clone())
            .map_err(|_| "Invalid saved-agent receipt.")?;
        if saved.definition != review.config
            || saved.deleted
            || (review.existing.is_none() && saved.revision != 1)
            || review.existing.as_ref().is_some_and(|v| {
                saved.id != v.id
                    || saved.revision
                        != if review.config == v.definition {
                            v.revision
                        } else {
                            v.revision.saturating_add(1)
                        }
            })
        {
            return Err(
                "Saved-agent receipt does not match the reviewed change. Inspect before retrying."
                    .into(),
            );
        }
        bound_review(&review.session, saved)
    }
    pub fn agent_delete(&mut self, review: &AgentReview, confirmed: bool) -> Result<()> {
        if !confirmed {
            return Err("Confirm deletion of the selected agent definition.".into());
        }
        self.agent_check(&review.session)?;
        consume(&review.used)?;
        let result = self.transport.request(
            "DELETE",
            &format!(
                "/api/agent/definitions/{}",
                encode_component(&review.definition.id)
            ),
            Some(&review.session),
            Some(json!({"expected_revision":review.definition.revision})),
        );
        self.agent_check(&review.session)?;
        let result = result?;
        if result["id"] != review.definition.id
            || result["revision"].as_u64() != review.definition.revision.checked_add(1)
            || result["deleted"] != true
            || result["task_history_preserved"] != true
        {
            return Err(
                "Deletion receipt is unconfirmed; inspect the agent list before retrying.".into(),
            );
        }
        Ok(())
    }
    pub fn agent_pin_model(&self, review: &LocalModelReview) -> Result<AgentModel> {
        if review.config() != &self.view.config {
            return Err("Review the current local model before pinning it.".into());
        }
        Ok(AgentModel::Pinned {
            provider: review.config().provider.clone(),
            endpoint: review.config().endpoint.clone(),
            model: review.config().model.clone(),
            scope_id: review.resource().id.clone(),
        })
    }
    pub fn agent_launch_review(
        &self,
        agent: &AgentReview,
        input: String,
        model: Option<&LocalModelReview>,
    ) -> Result<AgentLaunchReview> {
        let session = self.agent_session()?;
        if session != agent.session || input.trim().is_empty() || input.len() > 16384 {
            return Err("Review the saved agent and a request of 1–16384 bytes.".into());
        }
        let needs = agent
            .definition
            .definition
            .capabilities
            .contains(&Permission::ModelInference);
        if needs != model.is_some() {
            return Err("Review the selected local model when model inference is granted.".into());
        }
        if let Some(model) = model {
            if model.config() != &self.view.config
                || !agent
                    .definition
                    .definition
                    .resources
                    .contains(model.resource())
            {
                return Err(
                    "The selected local model is outside this agent's saved resource grant.".into(),
                );
            }
            if let AgentModel::Pinned {
                provider,
                endpoint,
                model: target,
                scope_id,
            } = &agent.definition.definition.model
            {
                if provider != &model.config().provider
                    || endpoint != &model.config().endpoint
                    || target != &model.config().model
                    || scope_id != &model.resource().id
                {
                    return Err(
                        "The selected local model differs from this saved agent's pinned target."
                            .into(),
                    );
                }
            }
        }
        Ok(AgentLaunchReview {
            agent: agent.clone(),
            input,
            request_id: new_task_request_id()?,
            model: model.cloned(),
        })
    }
    pub fn agent_launch(
        &mut self,
        review: &AgentLaunchReview,
        confirmed: bool,
    ) -> Result<TaskSnapshot> {
        if !confirmed {
            return Err("Confirm the reviewed agent and task request.".into());
        }
        self.agent_check(&review.agent.session)?;
        let mut body = json!({"request_id":review.request_id,"expected_revision":review.agent.definition.revision,"input":review.input,"confirmed":true});
        if let Some(model) = &review.model {
            let current = self.task_local_model(model.config())?;
            if current.binding_id() != model.binding_id() || current.resource() != model.resource()
            {
                return Err("The reviewed local model changed before task creation.".into());
            }
            let Config {
                revision,
                provider,
                endpoint,
                ..
            } = model.config();
            body["model_review"] = json!({"expected":{"revision":revision,"provider":provider,"endpoint":endpoint},"binding_id":model.binding_id()});
        }
        let response = self.transport.request(
            "POST",
            &format!(
                "/api/agent/definitions/{}/launch",
                encode_component(&review.agent.definition.id)
            ),
            Some(&review.agent.session),
            Some(body),
        );
        self.agent_check(&review.agent.session)?;
        let response = response?;
        if response["model_started"] != false
            || response["definition"]["id"] != review.agent.definition.id
            || response["definition"]["revision"] != review.agent.definition.revision
        {
            return Err("Agent launch receipt differs from the reviewed request; inspect tasks without replaying.".into());
        }
        serde_json::from_value(response).map_err(|_| "Invalid created-task snapshot.".into())
    }
}
