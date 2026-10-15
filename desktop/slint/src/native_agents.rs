use super::*;
use crate::{
    agents_bridge::*,
    extensions_bridge::{ExtensionCatalog, ExtensionKind, InstructionRef, InstructionSelection},
    task_bridge::{LocalModelReview, Permission, ResourceScope, WorkspaceSnapshot},
};
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub catalog: AgentCatalog,
    pub extensions: ExtensionCatalog,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub current: Option<AgentReview>,
    pub editor: Option<AgentEditor>,
    pub model: Option<LocalModelReview>,
    pub message: String,
}
#[derive(Default)]
pub(super) struct Draft {
    pub workspace: Option<WorkspaceSnapshot>,
    pub capabilities: Vec<Permission>,
    pub resources: Vec<ResourceScope>,
    pub extensions: InstructionSelection,
}
pub(super) enum Action {
    Refresh,
    New,
    Select(String),
    Save(AgentEditReview),
    Delete(AgentReview),
    Restore(AgentReview),
    Model(Config),
    Launch(AgentLaunchReview),
}
impl Action {
    pub fn populates(&self) -> bool {
        matches!(
            self,
            Self::New | Self::Select(_) | Self::Save(_) | Self::Delete(_) | Self::Restore(_)
        )
    }
    pub fn launches(&self) -> bool {
        matches!(self, Self::Launch(_))
    }
    pub fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
        p: &mut Panel,
        tasks: &mut tasks_ui::Panel,
    ) -> crate::bridge::Result<()> {
        match self {
            Self::Refresh => {}
            Self::New => {
                p.current = None;
                p.model = None;
                p.message.clear();
            }
            Self::Select(id) => {
                p.current = Some(
                    p.catalog.review(
                        p.catalog
                            .definitions
                            .iter()
                            .find(|v| v.id == id)
                            .ok_or("Refresh the saved-agent list first.")?,
                    )?,
                );
                p.model = None;
                p.message.clear();
            }
            Self::Save(review) => {
                p.current = Some(b.agent_save(&review, true)?);
                p.message = "Saved locally.".into();
            }
            Self::Delete(review) => {
                b.agent_delete(&review, true)?;
                p.current = None;
                p.message="Agent moved to deleted definitions. Show deleted agents to restore it; task history remains.".into();
            }
            Self::Restore(review) => {
                p.current = Some(b.agent_restore(&review)?);
                p.message = "Agent settings restored. No task was started.".into();
            }
            Self::Model(config) => {
                p.model = Some(b.task_local_model(&config)?);
            }
            Self::Launch(review) => {
                let created = b.agent_launch(&review, true)?;
                tasks_ui::Action::Select(created.task.id).execute(b, tasks)?;
                p.message = "Task created. Review and run its model explicitly in Activity.".into();
            }
        }
        p.catalog = b.agent_catalog()?;
        p.extensions = b.extension_catalog()?;
        p.workspaces = b.task_overview()?.workspaces;
        p.editor = Some(b.agent_editor()?);
        Ok(())
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, action: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.agents_pending {
        return;
    }
    s.agents_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Agents(action)));
}
fn fail(state: &Rc<RefCell<UiState>>, error: impl Into<String>) {
    state.borrow_mut().last_error = Some(error.into());
}
fn config(w: &MainWindow, s: &UiState) -> crate::bridge::Result<AgentConfig> {
    let workspace = s
        .agent_draft
        .workspace
        .as_ref()
        .ok_or("Select an explicitly granted workspace first.")?;
    let model = if w.global::<NativeAgents>().get_model_mode() == 0 {
        AgentModel::SelectedAtLaunch
    } else if let Some(model) = &s.agent_panel.model {
        AgentModel::Pinned {
            provider: model.config().provider.clone(),
            endpoint: model.config().endpoint.clone(),
            model: model.config().model.clone(),
            scope_id: model.resource().id.clone(),
        }
    } else {
        s.agent_panel
            .current
            .as_ref()
            .and_then(|r| match &r.definition().definition.model {
                m @ AgentModel::Pinned { .. } => Some(m.clone()),
                _ => None,
            })
            .ok_or("Review the selected local model before pinning it.")?
    };
    Ok(AgentConfig {
        name: w.get_agent_name().to_string(),
        system_prompt: w.get_agent_prompt().to_string(),
        soul: w.get_agent_soul().to_string(),
        workspace_id: workspace.id.clone(),
        policy_revision: workspace.policy_revision,
        capabilities: s.agent_draft.capabilities.clone(),
        resources: s.agent_draft.resources.clone(),
        model,
        extensions: match w.global::<NativeAgents>().get_instructions_mode() {
            0 => InstructionSelection::Enabled,
            1 => InstructionSelection::Explicit {
                references: Vec::new(),
            },
            _ => match &s.agent_draft.extensions {
                InstructionSelection::Explicit { references } => InstructionSelection::Explicit {
                    references: references.clone(),
                },
                _ => InstructionSelection::Explicit {
                    references: Vec::new(),
                },
            },
        },
    })
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeAgents>()
            .on_toggle_instruction(move |index| {
                let mut s = state.borrow_mut();
                if s.agents_pending {
                    return;
                }
                if let Some((reference, _)) = usize::try_from(index)
                    .ok()
                    .and_then(|i| instruction_options(&s).get(i).cloned())
                {
                    if !matches!(
                        &s.agent_draft.extensions,
                        InstructionSelection::Explicit { .. }
                    ) {
                        s.agent_draft.extensions = InstructionSelection::Explicit {
                            references: Vec::new(),
                        };
                    }
                    if let InstructionSelection::Explicit { references } =
                        &mut s.agent_draft.extensions
                    {
                        if let Some(at) = references.iter().position(|r| r == &reference) {
                            references.remove(at);
                        } else {
                            references.push(reference);
                        }
                    }
                    if let Some(w) = weak.upgrade() {
                        project_scope(&w, &s);
                    }
                }
            });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeAgents>()
            .on_refresh(move || enqueue(&state, &tx, Action::Refresh));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.on_new_agent(move || enqueue(&state, &tx, Action::New));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.on_select_agent(move |id| enqueue(&state, &tx, Action::Select(id.to_string())));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.on_save_agent(move || {
            let Some(w) = weak.upgrade() else { return };
            let result = {
                let s = state.borrow();
                config(&w, &s).and_then(|config| {
                    s.agent_panel
                        .editor
                        .as_ref()
                        .ok_or("Refresh the saved-agent editor first.")?
                        .edit(
                            s.agent_panel.current.as_ref(),
                            config,
                            s.agent_draft.workspace.as_ref().unwrap(),
                        )
                })
            };
            match result {
                Ok(r) => enqueue(&state, &tx, Action::Save(r)),
                Err(e) => fail(&state, e),
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.on_delete_agent(move |id| {
            let review = state
                .borrow()
                .agent_panel
                .current
                .clone()
                .filter(|r| r.definition().id == id.as_str());
            if let Some(review) = review {
                enqueue(&state, &tx, Action::Delete(review));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeAgents>().on_restore(move || {
            let review = state.borrow().agent_panel.current.clone();
            if let Some(review) = review {
                enqueue(&state, &tx, Action::Restore(review));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeAgents>().on_inspect_model(move || {
            let config = state.borrow().view.config.clone();
            enqueue(&state, &tx, Action::Model(config));
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeAgents>()
            .on_select_workspace(move |index| {
                let mut s = state.borrow_mut();
                if s.agents_pending {
                    return;
                }
                let workspace = usize::try_from(index)
                    .ok()
                    .and_then(|i| s.agent_panel.workspaces.get(i))
                    .cloned();
                s.agent_draft = Draft {
                    workspace,
                    extensions: s.agent_draft.extensions.clone(),
                    ..Default::default()
                };
                if let Some(w) = weak.upgrade() {
                    project_scope(&w, &s);
                }
            });
    }
    for resource in [false, true] {
        let state = state.clone();
        let weak = w.as_weak();
        let callback = move |index: i32| {
            let mut s = state.borrow_mut();
            if s.agents_pending {
                return;
            }
            let Some(workspace) = s.agent_draft.workspace.clone() else {
                return;
            };
            if let Ok(i) = usize::try_from(index) {
                if resource {
                    if let Some(v) = workspace.resources.get(i) {
                        if let Some(at) = s.agent_draft.resources.iter().position(|r| r == v) {
                            s.agent_draft.resources.remove(at);
                        } else {
                            s.agent_draft.resources.push(v.clone());
                        }
                    }
                } else if let Some(v) = workspace.capabilities.get(i) {
                    if let Some(at) = s.agent_draft.capabilities.iter().position(|c| c == v) {
                        s.agent_draft.capabilities.remove(at);
                    } else {
                        s.agent_draft.capabilities.push(*v);
                    }
                }
            }
            if let Some(w) = weak.upgrade() {
                project_scope(&w, &s);
            }
        };
        if resource {
            w.global::<NativeAgents>().on_toggle_resource(callback)
        } else {
            w.global::<NativeAgents>().on_toggle_capability(callback)
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        w.on_run_agent_foreground(move || {
            let Some(w) = weak.upgrade() else { return };
            let review = (|| {
                let s = state.borrow();
                let agent = s
                    .agent_panel
                    .current
                    .as_ref()
                    .ok_or("Save an agent first.")?;
                let needs = agent
                    .definition()
                    .definition
                    .capabilities
                    .contains(&Permission::ModelInference);
                s.agent_panel
                    .editor
                    .as_ref()
                    .ok_or("Refresh this agent first.")?
                    .launch(
                        agent,
                        w.get_agent_run_input().to_string(),
                        if needs {
                            s.agent_panel.model.as_ref()
                        } else {
                            None
                        },
                    )
            })();
            match review {
                Ok(r) => enqueue(&state, &tx, Action::Launch(r)),
                Err(e) => fail(&state, e),
            }
        });
    }
}
pub(super) fn populate(w: &MainWindow, s: &mut UiState) {
    let row = s.agent_panel.current.as_ref().map(|r| r.definition());
    w.set_editing_agent(row.map(|r| r.id.clone()).unwrap_or_default().into());
    w.set_agent_name(
        row.map(|r| r.definition.name.clone())
            .unwrap_or_default()
            .into(),
    );
    w.set_agent_prompt(
        row.map(|r| r.definition.system_prompt.clone())
            .unwrap_or_default()
            .into(),
    );
    w.set_agent_soul(
        row.map(|r| r.definition.soul.clone())
            .unwrap_or_default()
            .into(),
    );
    w.set_agent_run_input("".into());
    let config = row.map(|r| &r.definition);
    s.agent_draft = Draft {
        workspace: config
            .and_then(|c| {
                s.agent_panel
                    .workspaces
                    .iter()
                    .find(|w| w.id == c.workspace_id && w.policy_revision == c.policy_revision)
            })
            .cloned(),
        capabilities: config.map(|c| c.capabilities.clone()).unwrap_or_default(),
        resources: config.map(|c| c.resources.clone()).unwrap_or_default(),
        extensions: config.map(|c| c.extensions.clone()).unwrap_or_default(),
    };
    w.global::<NativeAgents>().set_model_mode(i32::from(
        config.is_some_and(|c| matches!(c.model, AgentModel::Pinned { .. })),
    ));
    w.global::<NativeAgents>()
        .set_instructions_mode(match &s.agent_draft.extensions {
            InstructionSelection::Enabled => 0,
            InstructionSelection::Explicit { references } if references.is_empty() => 1,
            _ => 2,
        });
    project_scope(w, s);
}
fn instruction_options(s: &UiState) -> Vec<(InstructionRef, String)> {
    let mut options = s
        .agent_panel
        .extensions
        .rows
        .iter()
        .filter(|r| r.enabled && !r.deleted && r.kind != ExtensionKind::Recipe)
        .map(|r| {
            (
                InstructionRef {
                    id: r.id.clone(),
                    revision: r.revision,
                },
                format!("{} · {} · revision {}", r.kind.label(), r.name, r.revision),
            )
        })
        .collect::<Vec<_>>();
    if let InstructionSelection::Explicit { references } = &s.agent_draft.extensions {
        for r in references {
            if !options.iter().any(|(known, _)| known == r) {
                options.push((
                    r.clone(),
                    format!(
                        "Unavailable saved reference {} · revision {}",
                        r.id, r.revision
                    ),
                ));
            }
        }
    }
    options
}
fn project_scope(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeAgents>();
    let workspace = s.agent_draft.workspace.as_ref();
    g.set_instructions(ModelRc::new(VecModel::from(instruction_options(s).into_iter().map(|(r,label)|AgentOption{label:label.into(),selected:matches!(&s.agent_draft.extensions,InstructionSelection::Explicit{references}if references.contains(&r))}).collect::<Vec<_>>())));
    g.set_workspace_index(
        workspace
            .and_then(|selected| {
                s.agent_panel.workspaces.iter().position(|v| {
                    v.id == selected.id && v.policy_revision == selected.policy_revision
                })
            })
            .map(|i| i as i32)
            .unwrap_or(-1),
    );
    g.set_capabilities(ModelRc::new(VecModel::from(
        workspace
            .map(|v| {
                v.capabilities
                    .iter()
                    .map(|permission| AgentOption {
                        label: serde_json::to_value(permission)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .replace('_', " ")
                            .into(),
                        selected: s.agent_draft.capabilities.contains(permission),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    )));
    g.set_resources(ModelRc::new(VecModel::from(
        workspace
            .map(|v| {
                v.resources
                    .iter()
                    .map(|resource| AgentOption {
                        label: format!(
                            "{} · {}",
                            serde_json::to_value(resource.kind)
                                .unwrap()
                                .as_str()
                                .unwrap(),
                            resource.id
                        )
                        .into(),
                        selected: s.agent_draft.resources.contains(resource),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    )));
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeAgents>();
    g.set_available(s.view.available);
    g.set_busy(s.agents_pending);
    g.set_deleted(
        s.agent_panel
            .current
            .as_ref()
            .is_some_and(|r| r.definition().deleted),
    );
    g.set_status(s.agent_panel.message.clone().into());
    g.set_workspaces(strings(
        s.agent_panel
            .workspaces
            .iter()
            .map(|v| {
                format!(
                    "{} · policy {}{}",
                    v.label,
                    v.policy_revision,
                    if v.available { "" } else { " · unavailable" }
                )
            })
            .collect(),
    ));
    g.set_model_detail(
        s.agent_panel
            .model
            .as_ref()
            .map(|m| format!("{} · {}", m.config().model, m.config().provider))
            .unwrap_or_else(|| "No local model reviewed".into())
            .into(),
    );
    w.set_agents(ModelRc::new(VecModel::from(
        s.agent_panel
            .catalog
            .definitions
            .iter()
            .filter(|r| r.deleted == g.get_show_deleted())
            .map(|r| AgentItem {
                id: r.id.clone().into(),
                name: r.definition.name.clone().into(),
                model: r.definition.model.label().into(),
                prompt: r.definition.system_prompt.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
    w.set_agent_saving(s.agents_pending);
    w.set_agent_save_status(s.agent_panel.message.clone().into());
    w.set_agent_save_is_error(false);
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.agent_panel = Default::default();
    s.agent_draft = Default::default();
    s.agents_pending = false;
    s.agents_requested = false;
    populate(w, s);
    project(w, s);
}
