use super::*;
use crate::task_bridge::*;
use serde_json::json;
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub overview: TaskOverview,
    pub workspace: Option<WorkspaceReview>,
    pub provision: Option<ProvisionReview>,
    pub task: Option<TaskReview>,
    pub approval: Option<ApprovalReview>,
    pub trace: Option<TracePage>,
    pub model: Option<LocalModelReview>,
    pub run: Option<ModelRunReview>,
    pub operation: Option<ModelOperationReview>,
    pub recovery: Option<ModelRunRecovery>,
    pub uncertain: bool,
}
pub(super) enum Action {
    Refresh,
    Workspace(WorkspaceSnapshot),
    Review(WorkspaceProposal, Option<LocalModelReview>),
    InspectModel(Config),
    ReviewRun(TaskReview, LocalModelReview, usize),
    SubmitRun(ModelRunReview),
    InspectOperation(TaskReview),
    Provision(ProvisionReview),
    Create(WorkspaceReview, String),
    Select(String),
    Start(TaskReview),
    Call(TaskReview, TaskAction),
    Decide(ApprovalReview, ApprovalDecision),
    Cancel(TaskReview),
    Resume(TaskReview, ResumeDecision),
    Trace(TaskReview, Option<u64>),
}
impl Panel {
    fn active(&self) -> bool {
        self.uncertain
            || self
                .operation
                .as_ref()
                .is_some_and(|o| !o.operation().is_terminal())
    }
    pub(super) fn poll<T: Transport>(&mut self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        let Some(old) = self
            .operation
            .clone()
            .filter(|o| !o.operation().is_terminal())
        else {
            return Ok(());
        };
        let operation = b.task_model_poll(&old)?;
        let snapshot = b.task_snapshot(&operation.task_id)?;
        self.select(b, snapshot)?;
        self.operation =
            Some(b.task_model_operation_review(self.task.as_ref().unwrap(), &operation)?);
        if operation.is_terminal() {
            self.overview = b.task_overview()?;
            self.trace = Some(b.task_trace(&operation.task_id, None, 50)?);
        }
        Ok(())
    }
    fn select<T: Transport>(
        &mut self,
        b: &Bridge<T>,
        snapshot: TaskSnapshot,
    ) -> crate::bridge::Result<()> {
        let task = b.task_review(&snapshot)?;
        if self
            .task
            .as_ref()
            .is_some_and(|old| old.snapshot().task.id != snapshot.task.id)
        {
            self.run = None;
            self.operation = None;
            self.recovery = None;
        }
        self.approval = b.task_approval_review(&task).ok();
        self.workspace = Some(b.task_workspace_review(&snapshot.workspace)?);
        self.task = Some(task);
        Ok(())
    }
}
impl Action {
    pub(super) fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
        p: &mut Panel,
    ) -> crate::bridge::Result<()> {
        if p.active()
            && matches!(
                &self,
                Self::Workspace(_)
                    | Self::InspectModel(_)
                    | Self::Review(_, _)
                    | Self::Provision(_)
                    | Self::Create(_, _)
                    | Self::Start(_)
                    | Self::Call(_, _)
                    | Self::ReviewRun(_, _, _)
            )
        {
            return Err("Finish or cancel the current model run first.".into());
        }
        let snapshot = match self {
            Self::Refresh => {
                p.overview = b.task_overview()?;
                if let Some(task) = &p.task {
                    Some(b.task_snapshot(&task.snapshot().task.id)?)
                } else {
                    None
                }
            }
            Self::Workspace(w) => {
                p.workspace = Some(b.task_workspace_review(&w)?);
                None
            }
            Self::Review(proposal, model) => {
                p.provision = Some(b.task_provision_review(proposal, model.as_ref())?);
                None
            }
            Self::InspectModel(config) => {
                p.model = Some(b.task_local_model(&config)?);
                p.provision = None;
                p.run = None;
                None
            }
            Self::ReviewRun(task, model, steps) => {
                if p.active() {
                    return Err("Inspect the existing model run before another submission.".into());
                }
                p.run = Some(b.task_model_run_review(&task, &model, steps)?);
                None
            }
            Self::SubmitRun(review) => {
                if p.active() {
                    return Err("An existing model run must be inspected first.".into());
                }
                let operation = match b.task_model_submit_checked(&review, true) {
                    Ok(operation) => operation,
                    Err(ModelSubmitFailure::NotSubmitted(message)) => {
                        p.run = None;
                        p.recovery = None;
                        p.uncertain = false;
                        return Err(message);
                    }
                    Err(ModelSubmitFailure::OutcomeUnknown { message, recovery }) => {
                        p.recovery = Some(*recovery);
                        p.uncertain = true;
                        return Err(message);
                    }
                };
                p.operation = Some(operation);
                p.run = None;
                p.recovery = None;
                p.uncertain = false;
                Some(b.task_snapshot(&review.task().snapshot().task.id)?)
            }
            Self::InspectOperation(task) => {
                if p.uncertain {
                    let recovery = p.recovery.as_ref().ok_or(
                        "The submission recovery identity is unavailable. Inspect the saved task and trace; do not replay it.",
                    )?;
                    p.operation = Some(b.task_model_recover(&task, recovery)?);
                    p.uncertain = false;
                    p.recovery = None;
                    p.run = None;
                } else {
                    let operations = b.task_model_operations(&task)?;
                    if let Some(operation) = operations.last() {
                        p.operation = Some(b.task_model_operation_review(&task, operation)?);
                        p.run = None;
                    }
                }
                Some(b.task_snapshot(&task.snapshot().task.id)?)
            }
            Self::Provision(r) => {
                let w = b.task_provision(&r, true)?;
                p.workspace = Some(b.task_workspace_review(&w)?);
                p.provision = None;
                p.overview = b.task_overview()?;
                None
            }
            Self::Create(w, input) => Some(b.task_create(&w, &input)?),
            Self::Select(id) => {
                if p.active() && p.task.as_ref().is_some_and(|t| t.snapshot().task.id != id) {
                    return Err(
                        "Finish or cancel the current model run before selecting another task."
                            .into(),
                    );
                }
                if p.task.as_ref().is_some_and(|t| t.snapshot().task.id != id) {
                    p.operation = None;
                    p.run = None;
                    p.recovery = None;
                }
                Some(b.task_snapshot(&id)?)
            }
            Self::Start(r) => Some(b.task_start(&r)?),
            Self::Call(r, a) => {
                let action = b.task_action_review(&r, &new_task_request_id()?, a)?;
                Some(b.task_action(&action)?)
            }
            Self::Decide(r, d) => Some(b.task_decide(&r, d, true)?),
            Self::Cancel(r) => Some(
                if let Some(operation) = p
                    .operation
                    .as_ref()
                    .filter(|o| !o.operation().is_terminal())
                {
                    b.task_model_cancel(&r, operation, "Cancelled from native Activity")?
                } else {
                    b.task_cancel(&r, "Cancelled from native Activity")?
                },
            ),
            Self::Resume(r, d) => Some(b.task_resume(&r, d, true)?),
            Self::Trace(r, after) => {
                p.trace = Some(b.task_trace(&r.snapshot().task.id, after, 50)?);
                None
            }
        };
        if let Some(snapshot) = snapshot {
            let failed = snapshot.operation_failed();
            p.select(b, snapshot)?;
            p.overview = b.task_overview()?;
            let task = p.task.as_ref().unwrap();
            p.trace = Some(b.task_trace(&task.snapshot().task.id, None, 50)?);
            if failed {
                return Err("The tool returned a failed outcome. Inspect the visible result and trace; it was not retried.".into());
            }
        }
        b.refresh_lists()?;
        Ok(())
    }
}
fn proposal(w: &MainWindow, s: &UiState) -> WorkspaceProposal {
    let ui = w.global::<NativeTasks>();
    let mut capabilities = vec![];
    for (yes, p) in [
        (ui.get_read(), Permission::Read),
        (ui.get_list(), Permission::List),
        (ui.get_write(), Permission::Write),
        (ui.get_control(), Permission::Control),
    ] {
        if yes {
            capabilities.push(p);
        }
    }
    let mut resources = vec![];
    if ui.get_use_model() {
        capabilities.push(Permission::ModelInference);
        if let Some(model) = &s.task_panel.model {
            resources.push(model.resource().clone());
        }
    }
    WorkspaceProposal {
        label: ui.get_label().into(),
        capabilities,
        resources,
    }
}
fn selected(s: &UiState) -> crate::bridge::Result<TaskReview> {
    s.task_panel
        .task
        .clone()
        .ok_or("Select a task first.".into())
}
pub(super) fn wire(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    let callback = |f: fn(&MainWindow, &UiState) -> crate::bridge::Result<Action>| {
        let weak = window.as_weak();
        let state = state.clone();
        let tx = tx.clone();
        move || {
            let Some(w) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.tasks_pending || !s.view.available {
                return;
            }
            match f(&w, &s) {
                Ok(action) => {
                    s.tasks_pending = true;
                    let _ = tx.send((s.epoch.current(), Command::Tasks(action)));
                }
                Err(e) => s.last_error = Some(e),
            }
        }
    };
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_activity_refresh(move || {
            let mut s = state.borrow_mut();
            if !s.view.available || s.tasks_pending {
                return;
            }
            s.tasks_pending = true;
            let _ = tx.send((s.epoch.current(), Command::Tasks(Action::Refresh)));
        });
    }
    let ui = window.global::<NativeTasks>();
    ui.on_refresh(callback(|_, _| Ok(Action::Refresh)));
    ui.on_review_workspace(callback(|w, s| {
        Ok(Action::Review(
            proposal(w, s),
            if w.global::<NativeTasks>().get_use_model() {
                Some(
                    s.task_panel
                        .model
                        .clone()
                        .ok_or("Review the selected local model first.")?,
                )
            } else {
                None
            },
        ))
    }));
    ui.on_inspect_model(callback(|_, s| {
        Ok(Action::InspectModel(s.view.config.clone()))
    }));
    ui.on_review_run(callback(|w, s| {
        Ok(Action::ReviewRun(
            selected(s)?,
            s.task_panel
                .model
                .clone()
                .ok_or("Review the selected local model first.")?,
            w.global::<NativeTasks>()
                .get_max_steps()
                .parse()
                .map_err(|_| "Enter 1–8 model steps.")?,
        ))
    }));
    ui.on_submit_run(callback(|w, s| {
        let r = s
            .task_panel
            .run
            .clone()
            .ok_or("Review the model run first.")?;
        if !w.global::<NativeTasks>().get_run_confirmed()
            || r.task().snapshot().task != selected(s)?.snapshot().task
            || r.model().config() != &s.view.config
            || w.global::<NativeTasks>()
                .get_max_steps()
                .parse::<usize>()
                .ok()
                != Some(r.max_steps())
        {
            return Err("Task, model or step limit changed. Review this run again.".into());
        }
        Ok(Action::SubmitRun(r))
    }));
    ui.on_inspect_operation(callback(|_, s| Ok(Action::InspectOperation(selected(s)?))));
    ui.on_provision(callback(|w, s| {
        let r = s
            .task_panel
            .provision
            .clone()
            .ok_or("Review workspace permissions first.")?;
        if !w.global::<NativeTasks>().get_confirmed() || r.proposal() != &proposal(w, s) {
            return Err("Workspace permissions changed. Review them again.".into());
        }
        Ok(Action::Provision(r))
    }));
    ui.on_create(callback(|w, s| {
        Ok(Action::Create(
            s.task_panel
                .workspace
                .clone()
                .ok_or("Select a workspace first.")?,
            w.global::<NativeTasks>().get_input().into(),
        ))
    }));
    ui.on_start(callback(|_, s| Ok(Action::Start(selected(s)?))));
    ui.on_list_files(callback(|_, s| {
        Ok(Action::Call(selected(s)?, TaskAction::List))
    }));
    ui.on_read_file(callback(|w, s| {
        Ok(Action::Call(
            selected(s)?,
            TaskAction::Read {
                path: w.global::<NativeTasks>().get_path().into(),
            },
        ))
    }));
    ui.on_write_file(callback(|w, s| {
        Ok(Action::Call(
            selected(s)?,
            TaskAction::Write {
                path: w.global::<NativeTasks>().get_path().into(),
                contents: w.global::<NativeTasks>().get_content().into(),
            },
        ))
    }));
    ui.on_complete(callback(|w, s| {
        Ok(Action::Call(
            selected(s)?,
            TaskAction::Complete {
                summary: w.global::<NativeTasks>().get_content().into(),
            },
        ))
    }));
    ui.on_approve(callback(|w, s| {
        if !w.global::<NativeTasks>().get_approval_confirmed() {
            return Err("Confirm the exact pending call first.".into());
        }
        Ok(Action::Decide(
            s.task_panel
                .approval
                .clone()
                .ok_or("No supported pending approval.")?,
            ApprovalDecision::Approve,
        ))
    }));
    ui.on_deny(callback(|_, s| {
        Ok(Action::Decide(
            s.task_panel
                .approval
                .clone()
                .ok_or("No supported pending approval.")?,
            ApprovalDecision::Deny,
        ))
    }));
    ui.on_cancel(callback(|_, s| Ok(Action::Cancel(selected(s)?))));
    ui.on_resume(callback(|_, s| {
        Ok(Action::Resume(
            selected(s)?,
            ResumeDecision::ContinueWithoutReplay,
        ))
    }));
    ui.on_clarify(callback(|w, s| {
        Ok(Action::Resume(
            selected(s)?,
            ResumeDecision::Clarification(w.global::<NativeTasks>().get_reply().into()),
        ))
    }));
    ui.on_trace_more(callback(|_, s| {
        Ok(Action::Trace(
            selected(s)?,
            s.task_panel.trace.as_ref().and_then(|t| t.next_after),
        ))
    }));
    for workspace in [true, false] {
        let state = state.clone();
        let tx = tx.clone();
        let handler = move |index: i32| {
            let mut s = state.borrow_mut();
            if s.tasks_pending || !s.view.available {
                return;
            }
            let action = if workspace {
                s.task_panel
                    .overview
                    .workspaces
                    .get(index as usize)
                    .cloned()
                    .map(Action::Workspace)
            } else {
                s.task_panel
                    .overview
                    .tasks
                    .get(index as usize)
                    .map(|t| Action::Select(t.id.clone()))
            };
            if let Some(action) = action {
                s.tasks_pending = true;
                let _ = tx.send((s.epoch.current(), Command::Tasks(action)));
            }
        };
        if workspace {
            ui.on_select_workspace(handler)
        } else {
            ui.on_select_task(handler)
        }
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_activity_select_task(move |id| {
            let mut s = state.borrow_mut();
            if s.tasks_pending {
                return;
            }
            s.tasks_pending = true;
            let _ = tx.send((s.epoch.current(), Command::Tasks(Action::Select(id.into()))));
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_activity_cancel_task(move |id| {
            let mut s = state.borrow_mut();
            if s.tasks_pending {
                return;
            }
            if let Some(r) = s
                .task_panel
                .task
                .clone()
                .filter(|r| r.snapshot().task.id == id.as_str())
            {
                s.tasks_pending = true;
                let _ = tx.send((s.epoch.current(), Command::Tasks(Action::Cancel(r))));
            } else {
                s.last_error = Some("Open and review the current task before cancellation.".into());
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        window.on_activity_answer_clarification(move |id, text| {
            let mut s = state.borrow_mut();
            if s.tasks_pending {
                return;
            }
            if let Some(r) = s
                .task_panel
                .task
                .clone()
                .filter(|r| r.snapshot().task.id == id.as_str())
            {
                s.tasks_pending = true;
                let _ = tx.send((
                    s.epoch.current(),
                    Command::Tasks(Action::Resume(
                        r,
                        ResumeDecision::Clarification(text.into()),
                    )),
                ));
            }
        });
    }
}
pub(super) fn project(window: &MainWindow, s: &mut UiState) {
    let ui = window.global::<NativeTasks>();
    let p = &s.task_panel;
    ui.set_available(s.view.available);
    ui.set_busy(s.tasks_pending || !s.view.available);
    ui.set_workspaces(strings(
        p.overview
            .workspaces
            .iter()
            .map(|w| w.label.clone())
            .collect(),
    ));
    ui.set_tasks(strings(
        p.overview
            .tasks
            .iter()
            .map(|t| format!("{}", t.input.chars().take(70).collect::<String>()))
            .collect(),
    ));
    if p.workspace.is_some() && !ui.get_selected_workspace() {
        ui.set_show_setup(false);
    }
    ui.set_selected_workspace(p.workspace.is_some());
    ui.set_workspace_detail(
        p.workspace
            .as_ref()
            .map(|w| {
                format!(
                    "{}\nPermissions: {:?}\n{}",
                    w.workspace().label,
                    w.workspace().capabilities,
                    if w.workspace().output_boundary == "plaintext_private_temporary_files" {
                        "Private temporary files; task records stay in local storage."
                    } else {
                        &w.workspace().output_boundary
                    }
                )
            })
            .unwrap_or_default()
            .into(),
    );
    let proposal_id = json!([
        p.provision.as_ref().map(|r| r.proposal()),
        proposal(window, s),
        s.view.config.revision
    ])
    .to_string();
    if s.task_proposal_identity != proposal_id {
        s.task_proposal_identity = proposal_id;
        ui.set_confirmed(false);
    }
    ui.set_proposal(
        p.provision
            .as_ref()
            .map(|r| {
                format!(
                    "{} · {:?}{}",
                    r.proposal().label,
                    r.proposal().capabilities,
                    if r.proposal()
                        .capabilities
                        .contains(&Permission::ModelInference)
                    {
                        "\nIncludes only the reviewed local model."
                    } else {
                        ""
                    }
                )
            })
            .unwrap_or_default()
            .into(),
    );
    let approval_id = p
        .approval
        .as_ref()
        .map(|r| {
            json!([
                r.task().snapshot().task.id,
                r.task().snapshot().task.revision,
                r.call()
            ])
            .to_string()
        })
        .unwrap_or_default();
    if s.task_approval_identity != approval_id {
        s.task_approval_identity = approval_id;
        ui.set_approval_confirmed(false);
    }
    let selected_id = p
        .task
        .as_ref()
        .map(|t| t.snapshot().task.id.clone())
        .unwrap_or_default();
    if selected_id != s.task_seen_id {
        s.task_seen_id = selected_id;
        if !s.task_seen_id.is_empty() {
            ui.set_new_task(false);
        }
    }
    ui.set_selected_task(p.task.is_some());
    let grants = p
        .task
        .as_ref()
        .map(|r| r.snapshot().capabilities())
        .unwrap_or(&[]);
    ui.set_can_read(grants.contains(&Permission::Read));
    ui.set_can_list(grants.contains(&Permission::List));
    ui.set_can_write(grants.contains(&Permission::Write));
    ui.set_can_control(grants.contains(&Permission::Control));
    ui.set_can_model(grants.contains(&Permission::ModelInference));
    ui.set_model_active(p.active());
    ui.set_model_uncertain(p.uncertain);
    ui.set_model_reviewed(
        p.model
            .as_ref()
            .is_some_and(|m| m.config() == &s.view.config),
    );
    ui.set_model_detail(
        p.model
            .as_ref()
            .map(|m| format!("{} · {}", m.config().model, m.config().provider))
            .unwrap_or_default()
            .into(),
    );
    let run_id = json!([
        p.run.as_ref().map(|r| json!([
            r.task().snapshot().task.id,
            r.task().snapshot().task.revision,
            r.model().binding_id(),
            r.max_steps()
        ])),
        p.task.as_ref().map(|t| t.snapshot().task.revision),
        s.view.config,
        ui.get_max_steps().as_str()
    ])
    .to_string();
    if s.task_run_identity != run_id {
        s.task_run_identity = run_id;
        ui.set_run_confirmed(false);
    }
    ui.set_run_review(p.run.as_ref().filter(|_| !p.uncertain).map(|r| format!("{}\nModel: {} · {}\nUp to {} steps. Tool permissions and write approval still apply.",r.task().snapshot().task.input,r.model().config().model,r.model().config().provider,r.max_steps())).unwrap_or_default().into());
    ui.set_operation(
        p.operation
            .as_ref()
            .map(|o| {
                let o = o.operation();
                let state = match o.phase {
                    ModelRunPhase::Running => "Processing",
                    ModelRunPhase::Succeeded => "Finished",
                    ModelRunPhase::Failed => "Failed",
                    ModelRunPhase::Cancelled => "Cancelled",
                };
                let evidence = if o.cleanup_unconfirmed {
                    "Cleanup is unconfirmed; inspect the local runtime before loading again."
                } else {
                    match o.cancellation {
                        Some(ModelCancellationEvidence::ExternalMayContinue) => {
                            "The external server may still be processing."
                        }
                        Some(ModelCancellationEvidence::OwnedGenerationDrained) => {
                            "Owned model generation drained."
                        }
                        Some(ModelCancellationEvidence::NoActiveGeneration) => {
                            "No model generation was active."
                        }
                        None if o.worker_drained => "Task worker finished.",
                        None => "Task worker is active.",
                    }
                };
                format!(
                    "{state} · {}\n{evidence}{}",
                    o.stop_reason.replace('_', " "),
                    o.error
                        .as_ref()
                        .map(|e| format!("\n{}", e.message))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default()
            .into(),
    );
    ui.set_pending(
        p.task
            .as_ref()
            .is_some_and(|r| matches!(r.snapshot().task.state, TaskState::Pending)),
    );
    ui.set_running(p.task.as_ref().is_some_and(|r| {
        matches!(
            r.snapshot().task.state,
            TaskState::Running { in_flight: None }
        )
    }));
    ui.set_approval(p.approval.is_some());
    ui.set_interrupted(
        p.task
            .as_ref()
            .is_some_and(|r| matches!(r.snapshot().task.state, TaskState::Interrupted { .. })),
    );
    ui.set_clarification(p.task.as_ref().is_some_and(|r| {
        matches!(
            r.snapshot().task.state,
            TaskState::NeedsClarification { .. }
        )
    }));
    ui.set_terminal(
        p.task
            .as_ref()
            .is_some_and(|r| r.snapshot().task.state.is_terminal()),
    );
    ui.set_detail(
        p.task
            .as_ref()
            .map(|r| {
                let task = &r.snapshot().task;
                let state = serde_json::to_value(&task.state).unwrap();
                format!(
                    "{}\nStatus: {}\n{}{}",
                    task.input,
                    if p.uncertain {
                        "Submission unconfirmed".into()
                    } else if p
                        .operation
                        .as_ref()
                        .is_some_and(|o| o.operation().phase == ModelRunPhase::Running)
                    {
                        "Generating".into()
                    } else {
                        state["kind"]
                            .as_str()
                            .unwrap_or("unknown")
                            .replace('_', " ")
                    },
                    task.last_result
                        .as_ref()
                        .map(|v| format!(
                            "Latest result: {}",
                            serde_json::to_string_pretty(v).unwrap()
                        ))
                        .unwrap_or_default(),
                    task.extensions
                        .as_ref()
                        .and_then(|v| v["items"].as_array())
                        .filter(|v| !v.is_empty())
                        .map(|items| format!(
                            "\nCaptured instructions: {}",
                            items
                                .iter()
                                .map(|v| format!(
                                    "{} (revision {})",
                                    v["name"].as_str().unwrap_or("Unnamed"),
                                    v["revision"]
                                        .as_u64()
                                        .map(|n| n.to_string())
                                        .unwrap_or_else(|| "unknown".into())
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default()
            .into(),
    );
    ui.set_call(
        p.approval
            .as_ref()
            .map(|r| serde_json::to_string_pretty(r.call()).unwrap())
            .unwrap_or_default()
            .into(),
    );
    ui.set_trace(
        p.trace
            .as_ref()
            .map(|t| serde_json::to_string_pretty(&t.events).unwrap())
            .unwrap_or_default()
            .into(),
    );
    ui.set_more(p.trace.as_ref().is_some_and(|t| t.next_after.is_some()));
    if let Some(r) = &p.task {
        let value = serde_json::to_value(&r.snapshot().task).unwrap();
        window.set_activity_selected_task(r.snapshot().task.id.clone().into());
        let mut detail = task_item(&value);
        if p.uncertain {
            detail.status = "Submission unconfirmed".into();
        } else if p
            .operation
            .as_ref()
            .is_some_and(|o| o.operation().phase == ModelRunPhase::Running)
        {
            detail.status = "Generating".into();
        }
        window.set_activity_detail(detail);
        window.set_activity_has_selection(true);
    }
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.task_panel = Default::default();
    s.tasks_pending = false;
    s.task_approval_identity.clear();
    s.task_seen_id.clear();
    s.task_run_identity.clear();
    s.task_proposal_identity.clear();
    let ui = w.global::<NativeTasks>();
    ui.set_input("".into());
    ui.set_label("".into());
    ui.set_path("".into());
    ui.set_content("".into());
    ui.set_reply("".into());
    ui.set_confirmed(false);
    ui.set_approval_confirmed(false);
    ui.set_run_confirmed(false);
    ui.set_use_model(false);
}
