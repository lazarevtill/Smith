use super::*;
use crate::extensions_bridge::*;
#[derive(Clone, Default)]
pub(super) struct Panel {
    pub catalog: ExtensionCatalog,
    pub current: Option<ExtensionReview>,
    pub editor: Option<ExtensionEditor>,
    pub kind: Option<ExtensionKind>,
    pub message: String,
    pub import: Option<ExtensionImport>,
    pub legacy: Option<LegacyExtensionPage>,
}
pub(super) enum Action {
    Refresh,
    New(ExtensionKind),
    Select(ExtensionSelection),
    Save(ExtensionEdit),
    Toggle(ExtensionSelection, bool),
    Remove(ExtensionSelection),
    PluginReview(ExtensionEditor, std::path::PathBuf),
    LegacyFirst(ExtensionEditor, ExtensionKind),
    LegacyNext(LegacyExtensionPage),
    LegacyReview(LegacyExtensionRow),
    Import(ExtensionImport, bool),
    DismissImport,
    Purge(ExtensionReview, String),
}
impl Action {
    pub fn populates(&self) -> bool {
        matches!(
            self,
            Self::Refresh
                | Self::New(_)
                | Self::Select(_)
                | Self::Save(_)
                | Self::Remove(_)
                | Self::Import(..)
                | Self::Purge(..)
        )
    }
    pub fn toggles(&self) -> bool {
        matches!(self, Self::Toggle(..))
    }
    pub fn execute<T: Transport>(
        self,
        b: &mut Bridge<T>,
        p: &mut Panel,
    ) -> crate::bridge::Result<()> {
        match self {
            Self::PluginReview(editor, directory) => {
                if p.import.as_ref().is_some_and(ExtensionImport::submitted) {
                    return Err(
                        "Resolve or dismiss the retained import before selecting another source."
                            .into(),
                    );
                }
                p.import = None;
                p.import = Some(b.extension_plugin_review(&editor, &directory)?);
                p.message = "Review the selected plugin. Import keeps it disabled.".into();
            }
            Self::LegacyFirst(editor, kind) => {
                p.legacy = None;
                p.legacy = Some(b.extension_legacy_first(&editor, kind)?);
            }
            Self::LegacyNext(page) => {
                p.legacy = Some(b.extension_legacy_next(&page)?);
            }
            Self::LegacyReview(row) => {
                if p.import.as_ref().is_some_and(ExtensionImport::submitted) {
                    return Err("Resolve or dismiss the retained import first.".into());
                }
                p.import = None;
                p.import = Some(b.extension_legacy_review(&row)?);
                p.message =
                    "Review this preserved original record. Import creates a disabled copy.".into();
            }
            Self::Import(review, retry) => {
                if p.import
                    .as_ref()
                    .is_none_or(|old| old.request_id() != review.request_id())
                {
                    return Err("The displayed import review changed.".into());
                }
                p.import = Some(review.clone());
                let receipt = b.extension_import(&review, retry)?;
                p.kind = Some(receipt.record().summary.kind);
                p.current = Some(receipt);
                p.import = None;
                p.message="Imported disabled. Enable it when you want later tasks to use its instructions.".into();
            }
            Self::DismissImport => {
                p.import = None;
                p.message="Import review dismissed. Any earlier uncertain write can be inspected in the refreshed list.".into();
            }
            Self::Purge(review, id) => {
                b.extension_purge(&review, &id)?;
                p.current = None;
                p.message="Definition permanently removed. Captured task history and import receipts remain.".into();
            }
            Self::Refresh => {
                p.current = None;
                p.message.clear();
            }
            Self::New(kind) => {
                p.current = None;
                p.kind = Some(kind);
                p.message.clear();
            }
            Self::Select(selected) => {
                let r = b.extension_open(&selected)?;
                p.kind = Some(r.record().summary.kind);
                p.current = Some(r);
                p.message.clear();
            }
            Self::Save(edit) => {
                let saved = b.extension_save(&edit)?;
                p.kind = Some(saved.record().summary.kind);
                p.current = Some(saved);
                p.message =
                    "Saved locally. New tasks capture the current enabled instructions.".into();
            }
            Self::Toggle(selected, enabled) => {
                let old = b.extension_open(&selected)?;
                let saved = b.extension_set_enabled(&old, enabled)?;
                if p.current
                    .as_ref()
                    .is_some_and(|v| v.record().summary.id == saved.record().summary.id)
                {
                    p.current = Some(saved);
                }
                p.message = if enabled {
                    "Enabled for later tasks. Existing tasks keep their captured instructions."
                } else {
                    "Disabled for later tasks. Existing tasks keep their captured instructions."
                }
                .into();
            }
            Self::Remove(selected) => {
                let old = b.extension_open(&selected)?;
                let restoring = old.record().summary.deleted;
                let receipt = if restoring {
                    b.extension_restore(&old)?
                } else {
                    b.extension_trash(&old)?
                };
                if p.current
                    .as_ref()
                    .is_some_and(|v| v.record().summary.id == receipt.record().summary.id)
                {
                    p.current = None;
                }
                p.message = if restoring {
                    "Restored with its previous enabled preference. No task was started."
                } else {
                    "Moved to Trash. Restore preserves the definition and its enabled preference."
                }
                .into();
            }
        }
        p.catalog = b.extension_catalog()?;
        p.editor = Some(b.extension_editor()?);
        Ok(())
    }
}
fn kind(name: &str) -> Option<ExtensionKind> {
    match name {
        "skill" | "skills" => Some(ExtensionKind::Skill),
        "method" | "methods" => Some(ExtensionKind::Method),
        "recipe" | "tools" => Some(ExtensionKind::Recipe),
        "plugin" | "plugins" => Some(ExtensionKind::Plugin),
        _ => None,
    }
}
fn kind_name(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Skill => "skill",
        ExtensionKind::Method => "method",
        ExtensionKind::Recipe => "recipe",
        ExtensionKind::Plugin => "plugin",
    }
}
fn enqueue(state: &Rc<RefCell<UiState>>, tx: &mpsc::Sender<(u64, Command)>, action: Action) {
    let mut s = state.borrow_mut();
    if !s.view.available || s.extensions_pending {
        return;
    }
    s.extensions_pending = true;
    let _ = tx.send((s.epoch.current(), Command::Extensions(action)));
}
fn selected(s: &UiState, id: &str) -> crate::bridge::Result<ExtensionSelection> {
    s.extension_panel.catalog.select(
        s.extension_panel
            .catalog
            .rows
            .iter()
            .find(|r| r.id == id)
            .ok_or("Refresh the extension list first.")?,
    )
}
fn optional(text: SharedString) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}
fn definition(
    w: &MainWindow,
    kind: ExtensionKind,
) -> crate::bridge::Result<(ExtensionDefinition, bool)> {
    let g = w.global::<NativeExtensions>();
    Ok(match kind {
        ExtensionKind::Skill => (
            ExtensionDefinition::Skill {
                name: w.get_skill_name().to_string(),
                description: w.get_skill_desc().to_string(),
                instructions: w.get_skill_instructions().to_string(),
                category: w.get_skill_category().to_string(),
            },
            w.get_skill_enabled(),
        ),
        ExtensionKind::Method => (
            ExtensionDefinition::Method {
                name: w.get_method_name().to_string(),
                description: w.get_method_desc().to_string(),
                instructions: w.get_method_instructions().to_string(),
            },
            w.get_method_enabled(),
        ),
        ExtensionKind::Recipe => (
            ExtensionDefinition::Recipe {
                name: w.get_recipe_name().to_string(),
                description: w.get_recipe_desc().to_string(),
                command: w.get_recipe_command().to_string(),
                parameters: if w.get_recipe_params().trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(w.get_recipe_params().as_str())
                        .map_err(|_| "Recipe parameters must be a JSON object.")?
                },
            },
            w.get_recipe_enabled(),
        ),
        ExtensionKind::Plugin => (
            ExtensionDefinition::Plugin {
                manifest_id: g.get_plugin_manifest_id().to_string(),
                name: g.get_plugin_name().to_string(),
                description: g.get_plugin_description().to_string(),
                version: g.get_plugin_version().to_string(),
                authors: g
                    .get_plugin_authors()
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
                homepage: optional(g.get_plugin_homepage()),
                license: optional(g.get_plugin_license()),
                instructions: Some(g.get_plugin_instructions().to_string()),
                instructions_file: optional(g.get_plugin_instructions_file()),
                tools: g
                    .get_plugin_tools()
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
            },
            g.get_plugin_enabled(),
        ),
    })
}
pub(super) fn wire(
    w: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>()
            .on_refresh(move || enqueue(&state, &tx, Action::Refresh));
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_new(move |name| {
            if let Some(k) = kind(&name) {
                enqueue(&state, &tx, Action::New(k));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_edit(move |id| {
            let result = selected(&state.borrow(), &id);
            match result {
                Ok(selection) => enqueue(&state, &tx, Action::Select(selection)),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        w.global::<NativeExtensions>().on_choose_plugin(move || {
            if state.borrow().extensions_pending {
                return;
            }
            let epoch = state.borrow().epoch.current();
            match crate::picker::choose(
                "Choose one local plugin directory containing plugin.json",
                true,
            ) {
                Ok(Some(path)) if state.borrow().epoch.accepts(epoch) => {
                    if let Some(w) = weak.upgrade() {
                        w.global::<NativeExtensions>()
                            .set_import_directory(path.into());
                    }
                }
                Ok(_) => {}
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_review_plugin(move || {
            let Some(w) = weak.upgrade() else { return };
            let editor = state.borrow().extension_panel.editor.clone();
            if let Some(editor) = editor {
                enqueue(
                    &state,
                    &tx,
                    Action::PluginReview(
                        editor,
                        w.global::<NativeExtensions>()
                            .get_import_directory()
                            .as_str()
                            .into(),
                    ),
                );
            }
        });
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_browse_original(move || {
            let Some(w) = weak.upgrade() else { return };
            let kind = match w.global::<NativeExtensions>().get_original_kind() {
                1 => ExtensionKind::Method,
                2 => ExtensionKind::Recipe,
                _ => ExtensionKind::Skill,
            };
            let editor = state.borrow().extension_panel.editor.clone();
            if let Some(editor) = editor {
                enqueue(&state, &tx, Action::LegacyFirst(editor, kind));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_next_original(move || {
            let page = state.borrow().extension_panel.legacy.clone();
            if let Some(page) = page {
                enqueue(&state, &tx, Action::LegacyNext(page));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>()
            .on_review_original(move |index| {
                let row = usize::try_from(index).ok().and_then(|i| {
                    state
                        .borrow()
                        .extension_panel
                        .legacy
                        .as_ref()
                        .and_then(|p| p.rows.get(i))
                        .cloned()
                });
                if let Some(row) = row {
                    enqueue(&state, &tx, Action::LegacyReview(row));
                }
            });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_import_reviewed(move || {
            let review = state.borrow().extension_panel.import.clone();
            if let Some(review) = review {
                let retry = review.submitted();
                enqueue(&state, &tx, Action::Import(review, retry));
            }
        });
    }
    {
        let state = state.clone();
        let tx = tx.clone();
        w.global::<NativeExtensions>()
            .on_dismiss_import(move || enqueue(&state, &tx, Action::DismissImport));
    }
    {
        let state = state.clone();
        let weak = w.as_weak();
        let tx = tx.clone();
        w.global::<NativeExtensions>().on_purge(move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.global::<NativeExtensions>().get_permanent() {
                return;
            }
            let review = state.borrow().extension_panel.current.clone();
            if let Some(review) = review {
                let id = review.record().summary.id.clone();
                w.global::<NativeExtensions>().set_permanent(false);
                enqueue(&state, &tx, Action::Purge(review, id));
            }
        });
    }
    for k in [
        ExtensionKind::Skill,
        ExtensionKind::Method,
        ExtensionKind::Recipe,
        ExtensionKind::Plugin,
    ] {
        let state = state.clone();
        let tx = tx.clone();
        let weak = w.as_weak();
        let save = move || {
            let Some(w) = weak.upgrade() else { return };
            let result = (|| {
                let (definition, enabled) = definition(&w, k)?;
                let s = state.borrow();
                let p = &s.extension_panel;
                p.editor
                    .as_ref()
                    .ok_or("Refresh the extension editor first.")?
                    .edit(
                        p.current.as_ref().filter(|r| r.record().summary.kind == k),
                        definition,
                        enabled,
                    )
            })();
            match result {
                Ok(edit) => enqueue(&state, &tx, Action::Save(edit)),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        match k {
            ExtensionKind::Skill => w.on_add_skill(save),
            ExtensionKind::Method => w.on_add_method(save),
            ExtensionKind::Recipe => w.on_add_recipe(save),
            ExtensionKind::Plugin => w.global::<NativeExtensions>().on_save_plugin(save),
        }
    }
    for remove in [false, true] {
        for k in [
            ExtensionKind::Skill,
            ExtensionKind::Method,
            ExtensionKind::Recipe,
            ExtensionKind::Plugin,
        ] {
            let state = state.clone();
            let tx = tx.clone();
            let callback = move |id: SharedString| {
                let action = (|| {
                    let s = state.borrow();
                    let selection = selected(&s, &id)?;
                    if remove {
                        Ok(Action::Remove(selection))
                    } else {
                        let enabled = !s
                            .extension_panel
                            .catalog
                            .rows
                            .iter()
                            .find(|r| r.id == id.as_str())
                            .ok_or("Refresh this row.")?
                            .enabled;
                        Ok(Action::Toggle(selection, enabled))
                    }
                })();
                match action {
                    Ok(a) => enqueue(&state, &tx, a),
                    Err(e) => state.borrow_mut().last_error = Some(e),
                }
            };
            match (k, remove) {
                (ExtensionKind::Skill, false) => w.on_toggle_skill(callback),
                (ExtensionKind::Skill, true) => w.on_remove_skill(callback),
                (ExtensionKind::Method, false) => w.on_toggle_method(callback),
                (ExtensionKind::Method, true) => w.on_remove_method(callback),
                (ExtensionKind::Recipe, false) => w.on_toggle_recipe(callback),
                (ExtensionKind::Recipe, true) => w.on_remove_recipe(callback),
                (ExtensionKind::Plugin, false) => {
                    w.global::<NativeExtensions>().on_toggle_plugin(callback)
                }
                (ExtensionKind::Plugin, true) => {
                    w.global::<NativeExtensions>().on_remove_plugin(callback)
                }
            }
        }
    }
}
pub(super) fn populate(w: &MainWindow, s: &UiState) {
    let p = &s.extension_panel;
    let g = w.global::<NativeExtensions>();
    g.set_permanent(false);
    let k = p
        .current
        .as_ref()
        .map(|r| r.record().summary.kind)
        .or(p.kind)
        .unwrap_or(ExtensionKind::Skill);
    let blank = match k {
        ExtensionKind::Skill => ExtensionDefinition::Skill {
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
            category: String::new(),
        },
        ExtensionKind::Method => ExtensionDefinition::Method {
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
        },
        ExtensionKind::Recipe => ExtensionDefinition::Recipe {
            name: String::new(),
            description: String::new(),
            command: String::new(),
            parameters: serde_json::json!({}),
        },
        ExtensionKind::Plugin => ExtensionDefinition::Plugin {
            manifest_id: String::new(),
            name: String::new(),
            description: String::new(),
            version: String::new(),
            authors: Vec::new(),
            homepage: None,
            license: None,
            instructions: None,
            instructions_file: None,
            tools: Vec::new(),
        },
    };
    let d = p
        .current
        .as_ref()
        .map(|r| &r.record().definition)
        .unwrap_or(&blank);
    let enabled = p
        .current
        .as_ref()
        .is_none_or(|r| r.record().summary.enabled);
    match d {
        ExtensionDefinition::Skill {
            name,
            description,
            instructions,
            category,
        } => {
            w.set_skill_name(name.into());
            w.set_skill_desc(description.into());
            w.set_skill_instructions(instructions.into());
            w.set_skill_category(category.into());
            w.set_skill_enabled(enabled);
            w.set_autonomy_ext_tab("skills".into());
        }
        ExtensionDefinition::Method {
            name,
            description,
            instructions,
        } => {
            w.set_method_name(name.into());
            w.set_method_desc(description.into());
            w.set_method_instructions(instructions.into());
            w.set_method_enabled(enabled);
            w.set_autonomy_ext_tab("methods".into());
        }
        ExtensionDefinition::Recipe {
            name,
            description,
            command,
            parameters,
        } => {
            w.set_recipe_name(name.into());
            w.set_recipe_desc(description.into());
            w.set_recipe_command(command.into());
            w.set_recipe_params(
                serde_json::to_string_pretty(parameters)
                    .unwrap_or_default()
                    .into(),
            );
            w.set_recipe_enabled(enabled);
            w.set_autonomy_ext_tab("tools".into());
        }
        ExtensionDefinition::Plugin {
            manifest_id,
            name,
            description,
            version,
            authors,
            homepage,
            license,
            instructions,
            instructions_file,
            tools,
        } => {
            g.set_plugin_manifest_id(manifest_id.into());
            g.set_plugin_name(name.into());
            g.set_plugin_description(description.into());
            g.set_plugin_version(version.into());
            g.set_plugin_authors(authors.join("\n").into());
            g.set_plugin_tools(tools.join("\n").into());
            g.set_plugin_homepage(homepage.as_deref().unwrap_or("").into());
            g.set_plugin_license(license.as_deref().unwrap_or("").into());
            g.set_plugin_instructions(instructions.as_deref().unwrap_or("").into());
            g.set_plugin_instructions_file(instructions_file.as_deref().unwrap_or("").into());
            g.set_plugin_enabled(enabled);
            w.set_autonomy_ext_tab("plugins".into());
        }
    }
}
pub(super) fn project(w: &MainWindow, s: &UiState) {
    let g = w.global::<NativeExtensions>();
    let p = &s.extension_panel;
    let busy = s.extensions_pending || !s.view.available;
    g.set_busy(busy);
    g.set_import_ready(p.import.is_some());
    g.set_import_uncertain(p.import.as_ref().is_some_and(ExtensionImport::submitted));
    g.set_import_summary(
        p.import
            .as_ref()
            .map(|i| {
                format!(
                    "{} · {}\n{}\nImported disabled; no execution or tool grants.",
                    i.definition().kind().label(),
                    i.definition().name(),
                    i.definition().description()
                )
            })
            .unwrap_or_default()
            .into(),
    );
    g.set_import_instructions(
        p.import
            .as_ref()
            .and_then(|i| i.definition().instructions())
            .unwrap_or("")
            .into(),
    );
    g.set_import_evidence(
        p.import
            .as_ref()
            .map(|i| serde_json::to_string_pretty(i.provenance()).unwrap_or_default())
            .unwrap_or_default()
            .into(),
    );
    g.set_original_rows(ModelRc::new(VecModel::from(
        p.legacy
            .as_ref()
            .map(|p| {
                p.rows
                    .iter()
                    .map(|r| SharedString::from(r.name.as_str()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    )));
    g.set_original_next(p.legacy.as_ref().is_some_and(LegacyExtensionPage::has_next));
    g.set_original_status(
        p.legacy
            .as_ref()
            .map(|p| {
                format!(
                    "Records {}–{} of {}",
                    p.offset + u64::from(!p.rows.is_empty()),
                    p.offset + p.rows.len() as u64,
                    p.total
                )
            })
            .unwrap_or_default()
            .into(),
    );
    g.set_purge_name(
        p.current
            .as_ref()
            .filter(|r| r.record().summary.deleted)
            .map(|r| r.record().summary.name.as_str())
            .unwrap_or("")
            .into(),
    );
    g.set_status(p.message.clone().into());
    g.set_editing_kind(
        p.current
            .as_ref()
            .map(|r| kind_name(r.record().summary.kind))
            .unwrap_or("")
            .into(),
    );
    g.set_deleted(
        p.current
            .as_ref()
            .is_some_and(|r| r.record().summary.deleted),
    );
    w.set_skill_adding(busy);
    w.set_method_adding(busy);
    w.set_recipe_adding(busy);
    let rows = p
        .catalog
        .rows
        .iter()
        .filter(|r| r.deleted == g.get_show_trash());
    let mut skills = Vec::new();
    let mut methods = Vec::new();
    let mut recipes = Vec::new();
    let mut plugins = Vec::new();
    for r in rows {
        let detail = format!(
            "Revision {} · {}{}",
            r.revision,
            r.description,
            if r.kind == ExtensionKind::Recipe {
                " · execution unavailable"
            } else {
                ""
            }
        );
        match r.kind {
            ExtensionKind::Skill => skills.push(SkillItem {
                id: r.id.clone().into(),
                name: r.name.clone().into(),
                detail: detail.into(),
                enabled: r.enabled,
            }),
            ExtensionKind::Method => methods.push(MethodItem {
                id: r.id.clone().into(),
                name: r.name.clone().into(),
                detail: detail.into(),
                enabled: r.enabled,
            }),
            ExtensionKind::Recipe => recipes.push(RecipeItem {
                id: r.id.clone().into(),
                name: r.name.clone().into(),
                detail: detail.into(),
                enabled: r.enabled,
            }),
            ExtensionKind::Plugin => plugins.push(ExtensionItem {
                id: r.id.clone().into(),
                name: r.name.clone().into(),
                detail: detail.into(),
                enabled: r.enabled,
            }),
        }
    }
    w.set_skills(ModelRc::new(VecModel::from(skills)));
    w.set_methods(ModelRc::new(VecModel::from(methods)));
    w.set_recipes(ModelRc::new(VecModel::from(recipes)));
    g.set_plugins(ModelRc::new(VecModel::from(plugins)));
}
pub(super) fn clear(w: &MainWindow, s: &mut UiState) {
    s.extension_panel = Default::default();
    s.extensions_pending = false;
    s.extensions_requested = false;
    let g = w.global::<NativeExtensions>();
    g.set_show_trash(false);
    g.set_import_directory("".into());
    g.set_import_open(false);
    g.set_permanent(false);
    populate(w, s);
    w.set_method_name("".into());
    w.set_method_desc("".into());
    w.set_method_instructions("".into());
    w.set_recipe_name("".into());
    w.set_recipe_desc("".into());
    w.set_recipe_command("".into());
    w.set_recipe_params("".into());
    let tab = w.get_autonomy_ext_tab();
    for k in [
        ExtensionKind::Skill,
        ExtensionKind::Method,
        ExtensionKind::Recipe,
        ExtensionKind::Plugin,
    ] {
        let mut p = Panel::default();
        p.kind = Some(k);
        s.extension_panel = p;
        populate(w, s);
    }
    s.extension_panel = Default::default();
    w.set_autonomy_ext_tab(tab);
    project(w, s);
}

pub(super) fn enabled(w: &MainWindow, s: &UiState) {
    if let Some(r) = &s.extension_panel.current {
        let value = r.record().summary.enabled;
        match r.record().summary.kind {
            ExtensionKind::Skill => w.set_skill_enabled(value),
            ExtensionKind::Method => w.set_method_enabled(value),
            ExtensionKind::Recipe => w.set_recipe_enabled(value),
            ExtensionKind::Plugin => w.global::<NativeExtensions>().set_plugin_enabled(value),
        }
    }
}
