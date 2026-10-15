use super::*;
use crate::profile_bridge::{
    OriginalKey, ProfilePhase, ProfileReview, ProfileStatus, ProfileWorkflow, ReviewAction,
    SourceSelection,
};
use std::path::Path;
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub supported: bool,
    pub status: Option<ProfileStatus>,
    pub review: Option<ProfileReview>,
    pub uncertain: bool,
    pub operating: bool,
    // Only a user-initiated copy transition suppresses the generic revocation banner.
    pub expected_transition: bool,
}
pub(super) struct Worker {
    workflow: Option<ProfileWorkflow>,
    pub panel: Panel,
    polling: bool,
    tick: u8,
}
pub(super) enum Action {
    Inspect(SourceSelection, OriginalKey),
    Import(ProfileReview, OriginalKey),
    ReviewDeactivation(SourceSelection),
    Deactivate(ProfileReview),
    Open,
    Refresh,
    Discard,
}
impl Worker {
    pub fn new(root: Option<&Path>) -> crate::bridge::Result<Self> {
        let workflow = root.map(ProfileWorkflow::new).transpose()?;
        Ok(Self {
            panel: Panel {
                supported: workflow.is_some(),
                ..Default::default()
            },
            workflow,
            polling: false,
            tick: 0,
        })
    }
    pub fn active(&self) -> bool {
        self.polling
    }
    pub fn poll<T: Transport>(&mut self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        if !self.polling {
            return Ok(());
        }
        self.tick = (self.tick + 1) % 4;
        if self.tick == 0 {
            self.refresh(b)?;
        }
        Ok(())
    }
    fn refresh<T: Transport>(&mut self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        let workflow = self
            .workflow
            .as_mut()
            .ok_or("Copied-profile import requires an owned workspace.")?;
        let status = workflow.status(b)?;
        self.polling =
            status.state == ProfilePhase::Operating || status.state == ProfilePhase::Locking;
        if status
            .operation
            .as_ref()
            .is_some_and(|op| op.kind == "legacy-preflight" && op.succeeded())
            && self.panel.review.is_none()
        {
            self.panel.review = workflow.review().ok();
        }
        self.panel.operating = self.polling;
        self.panel.status = Some(status);
        Ok(())
    }
    pub fn execute<T: Transport>(
        &mut self,
        b: &mut Bridge<T>,
        action: Action,
    ) -> crate::bridge::Result<()> {
        let workflow = self
            .workflow
            .as_mut()
            .ok_or("Copied-profile import requires an owned workspace.")?;
        match action {
            Action::Discard => {
                workflow.invalidate_review();
                self.panel.review = None;
                Ok(())
            }
            Action::Refresh => self.refresh(b),
            Action::Open => {
                workflow.open(b)?;
                self.panel.uncertain = false;
                self.panel.expected_transition = false;
                self.refresh(b)
            }
            Action::ReviewDeactivation(selection) => {
                b.save_draft()?;
                self.panel.review = Some(workflow.review_rollback(b, selection)?);
                Ok(())
            }
            Action::Inspect(selection, key) => {
                b.save_draft()?;
                self.panel.review = None;
                self.polling = true;
                self.panel.operating = true;
                self.panel.expected_transition = true;
                self.panel.uncertain = true;
                let result = workflow.inspect(b, selection, key);
                if result.is_ok() {
                    self.panel.uncertain = false;
                }
                result.map(|_| ())
            }
            Action::Import(review, key) => {
                b.save_draft()?;
                self.panel.review = None;
                self.polling = true;
                self.panel.operating = true;
                self.panel.expected_transition = true;
                self.panel.uncertain = true;
                let result = workflow.execute(b, review, key);
                if result.is_ok() {
                    self.panel.uncertain = false;
                }
                result.map(|_| ())
            }
            Action::Deactivate(review) => {
                b.save_draft()?;
                self.panel.review = None;
                self.polling = true;
                self.panel.operating = true;
                self.panel.expected_transition = true;
                self.panel.uncertain = true;
                let result = workflow.rollback(b, review);
                if result.is_ok() {
                    self.panel.uncertain = false;
                }
                result.map(|_| ())
            }
        }
    }
}
fn key(w: &MainWindow) -> crate::bridge::Result<OriginalKey> {
    let g = w.global::<NativeProfile>();
    let result = if g.get_key_kind() == 0 {
        let master = g.get_master().to_string();
        clear_keys(w);
        OriginalKey::master(master)
    } else {
        let database = g.get_database_key().to_string();
        let artifact = g.get_artifact_key().to_string();
        clear_keys(w);
        OriginalKey::derived(database, artifact)
    };
    result
}
pub(super) fn clear_keys(w: &MainWindow) {
    let g = w.global::<NativeProfile>();
    g.set_master("".into());
    g.set_database_key("".into());
    g.set_artifact_key("".into());
}
fn selection(w: &MainWindow) -> crate::bridge::Result<SourceSelection> {
    let g = w.global::<NativeProfile>();
    SourceSelection::new(
        Path::new(g.get_source().as_str()),
        Path::new(g.get_managed().as_str()),
        g.get_attested(),
    )
}
fn enqueue(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
    action: Action,
) {
    let mut s = state.borrow_mut();
    if s.profile_pending {
        return;
    }
    capture_draft(w, tx, &mut s);
    s.profile_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Profile(action)));
}
pub(super) fn wire(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    for source in [true, false] {
        let weak = window.as_weak();
        let state = state.clone();
        let callback = move || {
            let Some(w) = weak.upgrade() else { return };
            if state.borrow().profile_pending || state.borrow().profile_panel.review.is_some() {
                return;
            }
            let epoch = state.borrow().epoch.current();
            match crate::picker::choose(
                if source {
                    "Choose an independent stopped original profile copy"
                } else {
                    "Choose the independent original managed-files copy"
                },
                true,
            ) {
                Ok(Some(p)) if state.borrow().epoch.accepts(epoch) => {
                    if source {
                        w.global::<NativeProfile>().set_source(p.into())
                    } else {
                        w.global::<NativeProfile>().set_managed(p.into())
                    }
                }
                Ok(_) => (),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        if source {
            window.global::<NativeProfile>().on_choose_source(callback)
        } else {
            window.global::<NativeProfile>().on_choose_managed(callback)
        }
    }
    for index in 0..7 {
        let weak = window.as_weak();
        let state = state.clone();
        let tx = tx.clone();
        let callback = move || {
            let Some(w) = weak.upgrade() else { return };
            if state.borrow().profile_pending {
                return;
            }
            let action = (|| -> crate::bridge::Result<Action> {
                Ok(match index {
                    0 => {
                        let key = key(&w)?;
                        Action::Inspect(selection(&w)?, key)
                    }
                    1 => {
                        let key = key(&w)?;
                        if !w.global::<NativeProfile>().get_confirmed() {
                            return Err("Confirm the reviewed import first.".into());
                        }
                        Action::Import(
                            state
                                .borrow()
                                .profile_panel
                                .review
                                .clone()
                                .ok_or("Inspect the copied profile first.")?,
                            key,
                        )
                    }
                    2 => Action::ReviewDeactivation(selection(&w)?),
                    3 => {
                        if !w.global::<NativeProfile>().get_confirmed() {
                            return Err("Confirm deactivation first.".into());
                        }
                        Action::Deactivate(
                            state
                                .borrow()
                                .profile_panel
                                .review
                                .clone()
                                .ok_or("Review deactivation first.")?,
                        )
                    }
                    4 => Action::Open,
                    5 => Action::Refresh,
                    _ => Action::Discard,
                })
            })();
            match action {
                Ok(a) => {
                    w.global::<NativeProfile>().set_confirmed(false);
                    enqueue(&w, &state, &tx, a)
                }
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        let g = window.global::<NativeProfile>();
        match index {
            0 => g.on_inspect(callback),
            1 => g.on_import(callback),
            2 => g.on_inspect_deactivation(callback),
            3 => g.on_deactivate_import(callback),
            4 => g.on_open(callback),
            5 => g.on_refresh(callback),
            _ => g.on_discard(callback),
        }
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    if !s.view.available && s.profile_panel.expected_transition {
        // The Data pane shows operation state; real request errors still render afterward.
        w.set_status_text("".into());
    }
    let g = w.global::<NativeProfile>();
    let p = &s.profile_panel;
    g.set_supported(p.supported);
    g.set_workspace_open(s.view.available);
    g.set_busy(
        s.profile_pending
            || p.operating
            || p.status.as_ref().is_some_and(|v| {
                matches!(v.state, ProfilePhase::Operating | ProfilePhase::Locking)
            }),
    );
    g.set_can_open(
        !s.view.available
            && p.status
                .as_ref()
                .is_some_and(|v| v.state == ProfilePhase::Unlocked),
    );
    g.set_reviewed(p.review.is_some());
    g.set_deactivate(
        p.review
            .as_ref()
            .is_some_and(|r| r.action() == ReviewAction::Deactivate),
    );
    let status = p
        .status
        .as_ref()
        .map(|v| {
            if let Some(op) = &v.operation {
                format!(
                    "{} · {}{}",
                    match op.kind.as_str() {
                        "legacy-preflight" => "Copy inspection",
                        "legacy-import" => "Copy import",
                        "legacy-rollback" => "Import deactivation",
                        _ => "Workspace operation",
                    },
                    op.status,
                    op.error
                        .as_ref()
                        .map(|e| format!(" · {e}"))
                        .unwrap_or_default()
                )
            } else {
                format!("Workspace: {:?}", v.state)
            }
        })
        .unwrap_or_default();
    g.set_status(
        if p.uncertain {
            format!("Outcome not confirmed. Check status before continuing. {status}")
        } else {
            status
        }
        .into(),
    );
    let report = p
        .review
        .as_ref()
        .and_then(|v| v.report())
        .cloned()
        .or_else(|| {
            p.status
                .as_ref()
                .and_then(|v| v.operation.as_ref())
                .and_then(|v| v.import_report().ok())
        });
    g.set_report(report.map(|r|format!("Verified schema {} → {} · {} chats · {} messages · {} artifacts · {} tasks. {} retained tables; {} projection warnings.",r.source_schema,r.target_schema,r.mapped_chats,r.mapped_messages,r.mapped_artifacts,r.mapped_tasks,r.retained_tables,r.projection_warnings)).unwrap_or_default().into());
    if let Some(review) = &p.review {
        let current = g.get_report().to_string();
        g.set_report(
            format!(
                "{}\nOriginal copy: {}\nManaged copy: {}",
                current,
                review.selection().source_copy().display(),
                review.selection().managed_copy().display()
            )
            .into(),
        );
    }
}
