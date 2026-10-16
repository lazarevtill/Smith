use super::*;
use crate::{bridge::Result, catalog_bridge::*, catalog_controller::CatalogPanel};
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub core: CatalogPanel,
    pub settings: Option<crate::settings::Review>,
    pub mlx: Option<crate::mlx::MlxReview>,
    observation: Option<String>,
}
impl Panel {
    fn settings<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        b.refresh_settings()?;
        b.refresh_mlx()?;
        self.settings = b.settings_review().ok();
        self.mlx = b.mlx_review().ok();
        Ok(())
    }
    pub fn poll<T: Transport>(&mut self, b: &mut Bridge<T>) -> Result<()> {
        if let Some(id) = self.observation.clone() {
            b.refresh_settings()?;
            let o = &b.view.settings.setup["operation"];
            if o["id"] != id {
                self.observation = None;
                return Err(
                    "Computer observation changed. Refresh its current status in Inference.".into(),
                );
            }
            if o["status"] != "running" {
                self.observation = None;
                self.core.notice = if o["status"] == "succeeded" {
                    "Computer observation refreshed. Prepare selected files next.".into()
                } else {
                    o["error"]
                        .as_str()
                        .unwrap_or("Computer observation did not complete.")
                        .into()
                };
            }
        }
        self.core.poll(b)
    }
}
pub(super) enum Action {
    Refresh(bool),
    Details(EntryReview),
    Mutate(CatalogMutation),
    Inspect,
    Retry,
    Cancel,
    Prepare(PrepareReview),
    Observe(crate::settings::Review),
    Setup(PreparedReview, SetupSettings),
}
impl Action {
    pub fn execute<T: Transport>(self, b: &mut Bridge<T>, p: &mut Panel) -> Result<()> {
        match self {
            Self::Refresh(trash) => {
                p.core.refresh(b, trash)?;
                p.settings(b)
            }
            Self::Details(r) => {
                p.core.details = Some(b.catalog_details(&r)?);
                Ok(())
            }
            Self::Mutate(m) => p.core.submit(b, m),
            Self::Inspect => p.core.inspect(b),
            Self::Retry => p.core.retry(b),
            Self::Cancel => p.core.cancel(b),
            Self::Prepare(r) => p.core.prepare(b, r),
            Self::Observe(r) => {
                b.inventory(&r)?;
                p.settings(b)?;
                p.observation = b.view.settings.setup["operation"]["id"]
                    .as_str()
                    .map(str::to_owned);
                p.core.notice = "Observing this computer…".into();
                Ok(())
            }
            Self::Setup(r, settings) => {
                p.core.setup = Some(match &settings {
                    SetupSettings::Gguf { .. } => SetupAdapter::Gguf,
                    SetupSettings::Mlx { .. } => SetupAdapter::Mlx,
                });
                let outcome = b.catalog_setup_checked(&r, settings);
                match outcome {
                    Ok(op) => {
                        p.core.setup_attempted = true;
                        p.core.setup = Some(op.adapter);
                        p.core.notice="Setup inspection started. Review its result in Inference before loading.".into();
                        p.settings(b)
                    }
                    Err(SetupFailure::NotSubmitted(e)) => Err(e),
                    Err(SetupFailure::OutcomeUnknown(e)) => {
                        p.core.setup_attempted = true;
                        p.core.notice="Setup handoff outcome is unconfirmed. Inspect Inference status before another attempt.".into();
                        Err(e)
                    }
                }
            }
        }
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, action: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.catalog_pending {
        return;
    }
    s.catalog_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Catalog(action)));
}
fn row(s: &UiState, id: &str) -> Result<EntryReview> {
    let p = s
        .catalog_panel
        .core
        .page
        .as_ref()
        .ok_or("Refresh saved models first.")?;
    p.review(
        p.entries
            .iter()
            .find(|r| r.id == id)
            .ok_or("Choose a displayed saved model.")?,
    )
}
fn setup_settings(_w: &MainWindow, s: &UiState, r: &PreparedReview) -> Result<SetupSettings> {
    let platform = crate::platform_ui::Platform::current();
    if !platform.owned_runtime() || (r.entry().format == ModelFormat::Mlx && !platform.mlx()) {
        return Err("This saved model cannot run on this platform. Its saved files and settings are retained.".into());
    }
    match r.entry().format {
        ModelFormat::Gguf => Ok(SetupSettings::Gguf {
            reviewed: s
                .catalog_panel
                .settings
                .clone()
                .ok_or("Refresh model settings first.")?,
            model_id: r.runtime_model_id().to_owned(),
        }),
        ModelFormat::Mlx => Ok(SetupSettings::Mlx {
            reviewed: s
                .catalog_panel
                .mlx
                .clone()
                .ok_or("Refresh MLX settings first.")?,
            model_id: r.runtime_model_id().to_owned(),
            capability: s.view.mlx["settings"]["capability"]
                .as_str()
                .ok_or("Refresh saved MLX capability first.")?
                .into(),
            sampling: s.view.mlx["settings"]["sampling"].clone(),
            thinking_mode: serde_json::from_value(
                s.view.mlx["settings"]
                    .get("thinking_mode")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!("model_default")),
            )
            .map_err(|_| "Unsupported saved thinking preference.")?,
        }),
    }
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    let action = |f: fn(&MainWindow, &UiState) -> Result<Action>| {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        move || {
            let Some(w) = weak.upgrade() else { return };
            let a = f(&w, &state.borrow());
            match a {
                Ok(a) => enqueue(&state, &tx, a),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        }
    };
    let g = w.global::<NativeCatalog>();
    g.on_refresh(action(|w, _| {
        Ok(Action::Refresh(w.global::<NativeCatalog>().get_trash()))
    }));
    g.on_scope_changed(action(|w, _| {
        Ok(Action::Refresh(w.global::<NativeCatalog>().get_trash()))
    }));
    g.on_inspect(action(|_, _| Ok(Action::Inspect)));
    g.on_retry(action(|_, _| Ok(Action::Retry)));
    g.on_cancel(action(|_, _| Ok(Action::Cancel)));
    g.on_observe(action(|_, s| {
        Ok(Action::Observe(
            s.catalog_panel
                .settings
                .clone()
                .ok_or("Refresh model settings first.")?,
        ))
    }));
    g.on_continue_setup(action(|w, s| {
        let review = s
            .catalog_panel
            .core
            .prepared
            .clone()
            .ok_or("Prepare the saved entry first.")?;
        Ok(Action::Setup(
            review.clone(),
            setup_settings(w, s, &review)?,
        ))
    }));
    g.on_rename(action(|w, s| {
        let r = s
            .catalog_panel
            .core
            .details
            .as_ref()
            .ok_or("Open this saved entry's Details first.")?
            .review();
        Ok(Action::Mutate(r.rename(
            w.global::<NativeCatalog>().get_rename_label().into(),
        )?))
    }));
    g.on_save_gguf(action(|w, s| {
        let g = w.global::<NativeCatalog>();
        let selection = gguf_selection(
            std::path::Path::new(g.get_executable().as_str()),
            std::path::Path::new(g.get_model().as_str()),
        )?;
        let mutation = if let Some(review) = &s.catalog_replacement {
            review.replace(g.get_label().into(), selection)?
        } else {
            s.catalog_panel
                .core
                .page
                .as_ref()
                .ok_or("Refresh saved models first.")?
                .editor()
                .register(g.get_label().into(), selection)?
        };
        Ok(Action::Mutate(mutation))
    }));
    g.on_save_mlx(action(|w, s| {
        let selection = mlx_selection(
            s.catalog_panel
                .mlx
                .as_ref()
                .ok_or("Inspect a local MLX package in Inference first.")?,
        )?;
        let label = w.global::<NativeCatalog>().get_label().into();
        let mutation = if let Some(review) = &s.catalog_replacement {
            review.replace(label, selection)?
        } else {
            s.catalog_panel
                .core
                .page
                .as_ref()
                .ok_or("Refresh saved models first.")?
                .editor()
                .register(label, selection)?
        };
        Ok(Action::Mutate(mutation))
    }));
    {
        let state = state.clone();
        let weak = w.as_weak();
        g.on_new_entry(move || {
            let mut s = state.borrow_mut();
            if s.catalog_pending || s.catalog_panel.core.blocked() {
                return;
            }
            s.catalog_replacement = None;
            if let Some(w) = weak.upgrade() {
                let g = w.global::<NativeCatalog>();
                g.set_label("".into());
                g.set_executable("".into());
                g.set_model("".into());
                g.set_editor_open(true);
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        g.on_replace_files(move || {
            let mut s = state.borrow_mut();
            if s.catalog_pending || s.catalog_panel.core.blocked() {
                return;
            }
            let Some(d) = s.catalog_panel.core.details.clone() else {
                return;
            };
            if d.entry.trashed {
                return;
            }
            s.catalog_replacement = Some(d.review());
            if let Some(w) = weak.upgrade() {
                let g = w.global::<NativeCatalog>();
                g.set_label(d.entry.label.into());
                g.set_executable(
                    d.entry
                        .manifest
                        .runtime
                        .files
                        .iter()
                        .find(|f| f.role == FileRole::Executable)
                        .map(|f| {
                            d.entry
                                .manifest
                                .runtime
                                .root
                                .join(&f.name)
                                .to_string_lossy()
                                .into_owned()
                        })
                        .unwrap_or_default()
                        .into(),
                );
                g.set_model(
                    if d.entry.manifest.description.format == ModelFormat::Gguf {
                        d.entry
                            .manifest
                            .model
                            .files
                            .iter()
                            .find(|f| f.role == FileRole::Weights)
                            .map(|f| {
                                d.entry
                                    .manifest
                                    .model
                                    .root
                                    .join(&f.name)
                                    .to_string_lossy()
                                    .into_owned()
                            })
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                    .into(),
                );
                g.set_editor_open(true);
            }
        });
    }
    for kind in 0..5 {
        let state = state.clone();
        let tx = tx.clone();
        let callback = move |id: SharedString| {
            let a = (|| {
                let s = state.borrow();
                let r = row(&s, &id)?;
                Ok(match kind {
                    0 => Action::Details(r),
                    1 => Action::Mutate(r.select()?),
                    2 => Action::Mutate(r.trash()?),
                    3 => Action::Mutate(r.restore()?),
                    _ => Action::Prepare(
                        s.catalog_panel
                            .core
                            .page
                            .as_ref()
                            .unwrap()
                            .prepare_review(r.entry(), &s.view.config)?,
                    ),
                })
            })();
            match a {
                Ok(a) => enqueue(&state, &tx, a),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        match kind {
            0 => g.on_details_for(callback),
            1 => g.on_select(callback),
            2 => g.on_trash_entry(callback),
            3 => g.on_restore_entry(callback),
            _ => g.on_prepare(callback),
        }
    }
    for runtime in [true, false] {
        let state = state.clone();
        let weak = w.as_weak();
        let callback = move || {
            if state.borrow().catalog_pending || state.borrow().catalog_panel.core.blocked() {
                return;
            }
            let epoch = state.borrow().epoch.current();
            match crate::picker::choose(
                if runtime {
                    "Choose the local GGUF runtime executable"
                } else {
                    "Choose one local GGUF model file"
                },
                false,
            ) {
                Ok(Some(path)) if state.borrow().epoch.accepts(epoch) => {
                    if let Some(w) = weak.upgrade() {
                        if runtime {
                            w.global::<NativeCatalog>().set_executable(path.into())
                        } else {
                            w.global::<NativeCatalog>().set_model(path.into())
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        if runtime {
            g.on_pick_runtime(callback)
        } else {
            g.on_pick_model(callback)
        }
    }
    let weak = w.as_weak();
    g.on_open_inference(move || {
        if let Some(w) = weak.upgrade() {
            w.set_active_view(2);
            w.set_settings_tab(3);
        }
    });
}
pub(super) fn received(w: &MainWindow, s: &mut UiState, details: bool, setup: bool, success: bool) {
    if details && success {
        if let Some(d) = &s.catalog_panel.core.details {
            w.global::<NativeCatalog>()
                .set_rename_label(d.entry.label.clone().into());
            w.global::<NativeCatalog>().set_details_open(true);
        }
    }
    if setup {
        s.catalog_setup_engine = s.catalog_panel.core.setup.map(|adapter| match adapter {
            SetupAdapter::Gguf => 0,
            SetupAdapter::Mlx => 1,
        });
        if let Some(engine) = s.catalog_setup_engine {
            w.global::<NativeSettings>().set_engine_index(engine);
        }
        w.set_active_view(2);
        w.set_settings_tab(3);
        s.settings_requested = false;
        if success {
            s.settings_target = Some(s.view.config.clone());
            s.settings_revision = s.view.settings.setup["preferences"]["revision"].as_u64();
            s.mlx_target = Some(s.view.config.clone());
            s.mlx_revision = s.view.mlx["settings"]["revision"].as_u64();
        }
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeCatalog>();
    let p = &s.catalog_panel.core;
    g.set_busy(s.catalog_pending || !s.view.available);
    g.set_blocked(p.blocked() || s.catalog_panel.observation.is_some());
    if !s.catalog_pending {
        if let Some(page) = &p.page {
            g.set_trash(page.trashed);
        }
    }
    g.set_can_inspect(p.pending.is_some() || p.prepare_recovery.is_some() || p.operation.is_some());
    g.set_can_retry(
        p.pending.is_some() && p.operation.as_ref().is_none_or(|o| o.snapshot().terminal()),
    );
    g.set_can_cancel(
        p.operation
            .as_ref()
            .is_some_and(|o| o.snapshot().status == OperationPhase::Running),
    );
    g.set_prepared(p.prepared.is_some() && !p.setup_attempted);
    g.set_prepared_name(
        p.prepared
            .as_ref()
            .map(|r| r.entry().label.as_str())
            .unwrap_or("")
            .into(),
    );
    g.set_status(p.notice.clone().into());
    g.set_pending_id(
        p.pending
            .as_ref()
            .map(CatalogMutation::request_id)
            .unwrap_or("")
            .into(),
    );
    g.set_has_details(p.details.is_some());
    g.set_replacing(s.catalog_replacement.is_some());
    g.set_details(
        p.details
            .as_ref()
            .map(|d| serde_json::to_string_pretty(&d.entry).unwrap_or_default())
            .unwrap_or_default()
            .into(),
    );
    g.set_current(
        format!(
            "Current chat model: {} · {}",
            if s.view.config.model == "smith-demo" {
                "Demo"
            } else {
                &s.view.config.model
            },
            s.view.config.provider
        )
        .into(),
    );
    let query = g.get_query().to_lowercase();
    g.set_rows(ModelRc::new(VecModel::from(
        p.page
            .as_ref()
            .map(|p| {
                p.entries
                    .iter()
                    .filter(|r| r.label.to_lowercase().contains(&query))
                    .map(|r| CatalogItem {
                        id: r.id.clone().into(),
                        name: r.label.clone().into(),
                        format: match r.format {
                            ModelFormat::Gguf => "GGUF",
                            ModelFormat::Mlx => "MLX",
                        }
                        .into(),
                        selected: r.selected_for_review,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    )));
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.catalog_panel = Default::default();
    s.catalog_replacement = None;
    s.catalog_setup_engine = None;
    s.catalog_pending = false;
    s.catalog_requested = false;
    let g = w.global::<NativeCatalog>();
    g.set_query("".into());
    g.set_label("".into());
    g.set_executable("".into());
    g.set_model("".into());
    g.set_rename_label("".into());
    g.set_editor_open(false);
    g.set_details_open(false);
    project(w, s);
}
