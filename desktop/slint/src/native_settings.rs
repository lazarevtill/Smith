use super::*;
use crate::settings::{Review, SelectedFile};
use serde_json::{json, Value};

pub(super) enum Action {
    Reload,
    Refresh,
    Save(Config, String, String),
    Test(Config),
    Token(Config, Option<String>),
    SelectModel(Config, String),
    Preferences(Review, Value),
    Reset(Review, bool),
    Inventory(Review),
    Inspect(Review, SelectedFile, SelectedFile, String),
    Probe(Review),
    Apply(Review),
    Cancel(Config, Value, bool),
    Unload(Config, Value),
}
impl Action {
    pub(super) fn replaces_fields(&self) -> bool {
        matches!(self, Self::Reload | Self::Reset(_, _))
    }
    pub(super) fn adopts_revision(&self) -> bool {
        matches!(
            self,
            Self::Reload
                | Self::Save(..)
                | Self::Test(..)
                | Self::Token(..)
                | Self::SelectModel(..)
                | Self::Preferences(..)
                | Self::Reset(..)
        )
    }
    pub(super) fn execute<T: Transport>(self, bridge: &mut Bridge<T>) -> crate::bridge::Result<()> {
        match self {
            Self::Reload => {
                bridge.refresh_configuration()?;
                bridge.refresh_settings()
            }
            Self::Refresh => bridge.refresh_settings(),
            Self::Save(c, p, e) => bridge.save_connection(&c, p, e),
            Self::Test(c) => bridge.test_connection(&c),
            Self::Token(c, t) => bridge.session_token(&c, t),
            Self::SelectModel(c, m) => bridge.select_model(&c, &m),
            Self::Preferences(r, i) => bridge.save_preferences(&r, i),
            Self::Reset(r, b) => bridge.reset_preferences(&r, b),
            Self::Inventory(r) => bridge.inventory(&r),
            Self::Inspect(r, e, m, id) => bridge.inspect_files(&r, e, m, id),
            Self::Probe(r) => bridge.prepare_plan(&r),
            Self::Apply(r) => bridge.apply_plan(&r),
            Self::Cancel(c, o, n) => bridge.cancel_setup(&c, &o, n),
            Self::Unload(c, r) => bridge.unload(&c, &r),
        }
    }
}
fn provider(index: i32) -> &'static str {
    match index {
        1 => "ollama",
        2 => "openai",
        3 => "managed",
        4 => "mlx",
        _ => "mock",
    }
}
fn target(state: &UiState) -> crate::bridge::Result<Config> {
    let config = state
        .settings_target
        .clone()
        .ok_or("Reload saved settings first.")?;
    if config != state.view.config {
        return Err(
            "The saved destination changed. Reload and review it before this action.".into(),
        );
    }
    Ok(config)
}
fn review(state: &UiState) -> crate::bridge::Result<Review> {
    Ok(Review {
        config: target(state)?,
        settings_revision: state
            .settings_revision
            .ok_or("Reload saved preferences first.")?,
        selection: state.view.settings.setup["selection"].clone(),
        prepared: state.view.settings.setup["prepared"].clone(),
    })
}
fn numeric(text: SharedString, label: &str, min: u64, max: u64) -> crate::bridge::Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    text.parse::<u64>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .map(|n| json!(n))
        .ok_or_else(|| format!("{label} must be {min}–{max}, or empty for Auto."))
}
fn toggle(index: i32) -> Value {
    match index {
        1 => json!(true),
        2 => json!(false),
        _ => Value::Null,
    }
}
fn intent(window: &MainWindow, state: &UiState) -> crate::bridge::Result<Value> {
    let ui = window.global::<NativeSettings>();
    let mut value = state.view.settings.setup["preferences"]["intent"].clone();
    if !value.is_object() {
        return Err("Reload saved preferences first.".into());
    }
    value["mode"] = json!(if ui.get_mode_index() == 1 {
        "manual"
    } else {
        "auto"
    });
    value["preference"] = json!(match ui.get_preference_index() {
        1 => "fast",
        2 => "quiet",
        _ => "balanced",
    });
    value["local_fallback"] = json!(match ui.get_fallback_index() {
        0 => "never",
        2 => "allow_verified_local",
        _ => "ask",
    });
    value["overrides"]["backend"] = match ui.get_backend_index() {
        1 => json!("llama_cpu"),
        2 => json!("llama_metal"),
        3 => json!("llama_cuda"),
        4 => json!("llama_hip"),
        5 => json!("llama_sycl"),
        6 => json!("llama_vulkan"),
        7 => json!("mlx_metal"),
        _ => Value::Null,
    };
    value["overrides"]["context_tokens"] = numeric(ui.get_context(), "Context", 128, 1_048_576)?;
    value["overrides"]["threads"] = numeric(ui.get_threads(), "Threads", 1, 1024)?;
    value["overrides"]["gpu_layers"] = numeric(ui.get_layers(), "GPU layers", 0, 10000)?;
    value["overrides"]["output_tokens"] = numeric(ui.get_output(), "Output tokens", 1, 1_048_576)?;
    value["overrides"]["prefill_threads"] =
        numeric(ui.get_prefill_threads(), "Prefill threads", 1, 1024)?;
    value["overrides"]["batch_tokens"] = numeric(ui.get_batch(), "Batch", 1, 65536)?;
    value["overrides"]["microbatch_tokens"] = numeric(ui.get_microbatch(), "Microbatch", 1, 65536)?;
    value["overrides"]["host_budget_bytes"] = numeric(
        ui.get_host_budget(),
        "Host budget",
        0,
        9_007_199_254_740_991,
    )?;
    value["overrides"]["device_budget_bytes"] = numeric(
        ui.get_device_budget(),
        "Device budget",
        0,
        9_007_199_254_740_991,
    )?;
    value["overrides"]["headroom_bytes"] =
        numeric(ui.get_headroom(), "Headroom", 0, 9_007_199_254_740_991)?;
    let device = ui.get_device().trim().to_owned();
    if device.len() > 160
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err(
            "Device ID must be at most 160 letters, numbers, dots, underscores, colons or hyphens."
                .into(),
        );
    }
    value["overrides"]["device_id"] = if device.is_empty() {
        Value::Null
    } else {
        json!(device)
    };
    value["overrides"]["kv_type"] = match ui.get_kv_index() {
        1 => json!("f16"),
        2 => json!("q8"),
        3 => json!("q4"),
        4 => json!("f32"),
        5 => json!("bf16"),
        _ => Value::Null,
    };
    value["overrides"]["flash_attention"] = toggle(ui.get_flash_index());
    value["overrides"]["mmap"] = toggle(ui.get_mmap_index());
    value["overrides"]["mlock"] = toggle(ui.get_mlock_index());
    Ok(value)
}
fn require_saved_form(window: &MainWindow, state: &UiState) -> crate::bridge::Result<Config> {
    let c = target(state)?;
    let ui = window.global::<NativeSettings>();
    if provider(ui.get_provider_index()) != c.provider
        || (matches!(c.provider.as_str(), "ollama" | "openai")
            && ui.get_endpoint().as_str() != c.endpoint)
    {
        return Err(
            "Save the edited connection before testing it or changing its token/model.".into(),
        );
    }
    Ok(c)
}
fn make_action(name: &str, window: &MainWindow, state: &UiState) -> crate::bridge::Result<Action> {
    let ui = window.global::<NativeSettings>();
    Ok(match name {
        "reload" => Action::Reload,
        "refresh" => Action::Refresh,
        "save" => Action::Save(
            target(state)?,
            provider(ui.get_provider_index()).into(),
            ui.get_endpoint().to_string(),
        ),
        "test" => Action::Test(require_saved_form(window, state)?),
        "set_token" => Action::Token(
            require_saved_form(window, state)?,
            Some(ui.get_token().to_string()),
        ),
        "clear_token" => Action::Token(require_saved_form(window, state)?, None),
        "preferences" => Action::Preferences(review(state)?, intent(window, state)?),
        "reset" => Action::Reset(review(state)?, false),
        "rollback" => Action::Reset(review(state)?, true),
        "inventory" => Action::Inventory(review(state)?),
        "inspect" => Action::Inspect(
            review(state)?,
            SelectedFile {
                path: ui.get_executable().to_string(),
                source: ui.get_source().to_string(),
                source_revision: ui.get_source_revision().to_string(),
            },
            SelectedFile {
                path: ui.get_model().to_string(),
                source: ui.get_source().to_string(),
                source_revision: ui.get_source_revision().to_string(),
            },
            ui.get_model_id().to_string(),
        ),
        "probe" if ui.get_probe_confirmed() && ui.get_can_probe() => Action::Probe(review(state)?),
        "apply" if ui.get_load_confirmed() && ui.get_can_load() => Action::Apply(review(state)?),
        "cancel" => {
            let native = state.view.settings.native["operation"]["status"] == "running";
            Action::Cancel(
                target(state)?,
                if native {
                    state.view.settings.native["operation"].clone()
                } else {
                    state.view.settings.setup["operation"].clone()
                },
                native,
            )
        }
        "unload" => Action::Unload(
            target(state)?,
            state.view.settings.native["runtime"].clone(),
        ),
        _ => return Err("Review and confirm the exact displayed plan first.".into()),
    })
}
pub(super) fn wire(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    macro_rules! connect {
        ($method:ident,$name:literal) => {{
            let state = state.clone();
            let tx = tx.clone();
            let weak = window.as_weak();
            window.global::<NativeSettings>().$method(move || {
                let Some(w) = weak.upgrade() else { return };
                let mut s = state.borrow_mut();
                if s.settings_pending || !s.view.available {
                    return;
                }
                match make_action($name, &w, &s) {
                    Ok(action) => {
                        if $name == "set_token" || $name == "reload" {
                            w.global::<NativeSettings>().set_token("".into());
                        }
                        if matches!($name, "inspect" | "probe" | "apply") {
                            w.global::<NativeSettings>().set_probe_confirmed(false);
                            w.global::<NativeSettings>().set_load_confirmed(false);
                        }
                        s.settings_pending = true;
                        w.global::<NativeSettings>().set_busy(true);
                        let _ = tx.send((s.epoch.current(), Command::Settings(action)));
                    }
                    Err(error) => {
                        w.global::<NativeSettings>().set_token("".into());
                        s.last_error = Some(error.clone());
                        w.set_status_text(error.into());
                    }
                }
            });
        }};
    }
    for executable in [true, false] {
        let state = state.clone();
        let weak = window.as_weak();
        let callback = move || {
            let Some(window) = weak.upgrade() else { return };
            let epoch = {
                let s = state.borrow();
                if !s.view.available || s.settings_pending {
                    return;
                }
                s.epoch.current()
            };
            let selected = crate::picker::file(if executable {
                "Choose an existing runtime executable"
            } else {
                "Choose an existing GGUF model"
            });
            let mut s = state.borrow_mut();
            if !s.view.available || !s.epoch.accepts(epoch) {
                return;
            }
            match selected {
                Ok(Some(path)) => {
                    if executable {
                        window
                            .global::<NativeSettings>()
                            .set_executable(path.into())
                    } else {
                        window.global::<NativeSettings>().set_model(path.into())
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    s.last_error = Some(error.clone());
                    window.set_status_text(error.into());
                }
            }
        };
        if executable {
            window
                .global::<NativeSettings>()
                .on_pick_executable(callback)
        } else {
            window.global::<NativeSettings>().on_pick_model(callback)
        }
    }
    connect!(on_reload, "reload");
    connect!(on_refresh, "refresh");
    connect!(on_save, "save");
    connect!(on_test, "test");
    connect!(on_set_token, "set_token");
    connect!(on_clear_token, "clear_token");
    connect!(on_preferences, "preferences");
    connect!(on_reset, "reset");
    connect!(on_rollback, "rollback");
    connect!(on_inventory, "inventory");
    connect!(on_inspect, "inspect");
    connect!(on_probe, "probe");
    connect!(on_apply, "apply");
    connect!(on_cancel, "cancel");
    connect!(on_unload, "unload");
    let state = state.clone();
    let tx = tx.clone();
    let weak = window.as_weak();
    window
        .global::<NativeSettings>()
        .on_select_model(move |model| {
            let Some(w) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.settings_pending {
                return;
            }
            match require_saved_form(&w, &s) {
                Ok(config) => {
                    s.settings_pending = true;
                    let _ = tx.send((
                        s.epoch.current(),
                        Command::Settings(Action::SelectModel(config, model.to_string())),
                    ));
                }
                Err(e) => s.last_error = Some(e),
            }
        });
}
pub(super) fn populate(window: &MainWindow, state: &mut UiState) {
    let ui = window.global::<NativeSettings>();
    let c = &state.view.config;
    ui.set_provider_index(match c.provider.as_str() {
        "ollama" => 1,
        "openai" => 2,
        "managed" => 3,
        "mlx" => 4,
        _ => 0,
    });
    ui.set_engine_index(
        state
            .catalog_setup_engine
            .take()
            .unwrap_or(if c.provider == "mlx" { 1 } else { 0 }),
    );
    ui.set_endpoint(c.endpoint.clone().into());
    ui.set_token("".into());
    let preferences = &state.view.settings.setup["preferences"];
    state.settings_target = Some(c.clone());
    state.settings_revision = preferences["revision"].as_u64();
    let value = &preferences["intent"];
    ui.set_mode_index(if value["mode"] == "manual" { 1 } else { 0 });
    ui.set_preference_index(match value["preference"].as_str() {
        Some("fast") => 1,
        Some("quiet") => 2,
        _ => 0,
    });
    ui.set_fallback_index(match value["local_fallback"].as_str() {
        Some("never") => 0,
        Some("allow_verified_local") => 2,
        _ => 1,
    });
    ui.set_backend_index(match value["overrides"]["backend"].as_str() {
        Some("llama_cpu") => 1,
        Some("llama_metal") => 2,
        Some("llama_cuda") => 3,
        Some("llama_hip") => 4,
        Some("llama_sycl") => 5,
        Some("llama_vulkan") => 6,
        Some("mlx_metal") => 7,
        _ => 0,
    });
    let number = |key: &str| {
        value["overrides"][key]
            .as_u64()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into()
    };
    ui.set_context(number("context_tokens"));
    ui.set_threads(number("threads"));
    ui.set_layers(number("gpu_layers"));
    ui.set_output(number("output_tokens"));
    ui.set_prefill_threads(number("prefill_threads"));
    ui.set_batch(number("batch_tokens"));
    ui.set_microbatch(number("microbatch_tokens"));
    ui.set_host_budget(number("host_budget_bytes"));
    ui.set_device_budget(number("device_budget_bytes"));
    ui.set_headroom(number("headroom_bytes"));
    ui.set_device(
        value["overrides"]["device_id"]
            .as_str()
            .unwrap_or("")
            .into(),
    );
    ui.set_kv_index(match value["overrides"]["kv_type"].as_str() {
        Some("f16") => 1,
        Some("q8") => 2,
        Some("q4") => 3,
        Some("f32") => 4,
        Some("bf16") => 5,
        _ => 0,
    });
    let tri = |key: &str| match value["overrides"][key].as_bool() {
        Some(true) => 1,
        Some(false) => 2,
        None => 0,
    };
    ui.set_flash_index(tri("flash_attention"));
    ui.set_mmap_index(tri("mmap"));
    ui.set_mlock_index(tri("mlock"));
}
pub(super) fn project(window: &MainWindow, state: &mut UiState) {
    let ui = window.global::<NativeSettings>();
    let setup = &state.view.settings.setup;
    let native = &state.view.settings.native;
    ui.set_available(state.view.available && !setup.is_null());
    let running =
        setup["operation"]["status"] == "running" || native["operation"]["status"] == "running";
    ui.set_busy(state.settings_pending || running);
    ui.set_models(strings(state.view.models.clone()));
    ui.set_selected_model(state.view.config.model.clone().into());
    ui.set_connection_status(
        format!(
            "Saved connection: {} · {} · {}",
            state.view.config.provider,
            if matches!(state.view.config.provider.as_str(), "managed" | "mlx") {
                state.view.config.model.as_str()
            } else {
                state.view.config.endpoint.as_str()
            },
            if state.view.config.tested {
                "tested"
            } else {
                "test required"
            }
        )
        .into(),
    );
    let pretty = |v: &Value| {
        if v.is_null() {
            String::new()
        } else {
            serde_json::to_string_pretty(v).unwrap_or_default()
        }
    };
    ui.set_saved_intent(pretty(&setup["preferences"]["intent"]).into());
    ui.set_inventory_status(pretty(&setup["inventory"]).into());
    ui.set_selection_details(pretty(&setup["selection"]).into());
    let files = &setup["selection"]["files"];
    ui.set_selection_review(
        if setup["selection"].is_null() {
            String::new()
        } else {
            format!(
                "Runtime: {}\nModel: {}\nChecks: --help and --list-devices",
                files["executable"]["path"].as_str().unwrap_or("unknown"),
                files["model"]["path"].as_str().unwrap_or("unknown")
            )
        }
        .into(),
    );
    ui.set_plan_details(
        if setup["prepared"].is_null() {
            String::new()
        } else {
            format!(
                "{}\n{}",
                pretty(&setup["preview"]),
                pretty(&setup["prepared"])
            )
        }
        .into(),
    );
    let plan = &setup["prepared"]["plan"];
    let config = &plan["config"];
    ui.set_plan_review(if plan.is_null(){String::new()}else{format!("{}\nRuntime: {}\nModel: {}\nContext {} · output {} · threads {}\nBackend: {}\nMemory fit and compute placement: unverified",plan["manifest"]["model_id"].as_str().unwrap_or("Local model"),plan["manifest"]["executable"]["path"].as_str().unwrap_or("reviewed selection"),plan["manifest"]["model"]["path"].as_str().unwrap_or("reviewed selection"),config["context_tokens"],setup["prepared"]["output_tokens"],config["threads"],config["backend"])}.into());
    let operation = |v: &Value| {
        if v.is_null() {
            String::new()
        } else {
            format!(
                "{}: {}{}",
                v["kind"].as_str().unwrap_or("Operation"),
                v["status"].as_str().unwrap_or("unknown"),
                v["error"]
                    .as_str()
                    .map(|e| format!(" · {e}"))
                    .unwrap_or_default()
            )
        }
    };
    ui.set_operation_status(
        format!(
            "{}\n{}",
            operation(&setup["operation"]),
            operation(&native["operation"])
        )
        .trim()
        .into(),
    );
    ui.set_runtime_status(if native["runtime"].is_null() {
        "No owned model is loaded.".into()
    } else {
        format!(
            "Owned local model: {} · {} · observed placement: Unknown",
            native["runtime"]["model_id"].as_str().unwrap_or("none"),
            native["runtime"]["phase"].as_str().unwrap_or("unknown")
        )
        .into()
    });
    let probe_identity = json!([
        state.view.config.revision,
        setup["preferences"]["revision"],
        setup["selection"],
        ui.get_executable().as_str(),
        ui.get_model().as_str(),
        ui.get_model_id().as_str(),
        ui.get_source().as_str(),
        ui.get_source_revision().as_str()
    ])
    .to_string();
    let load_identity = json!([
        state.view.config.revision,
        setup["preferences"]["revision"],
        setup["prepared"],
        ui.get_executable().as_str(),
        ui.get_model().as_str(),
        ui.get_model_id().as_str(),
        ui.get_source().as_str(),
        ui.get_source_revision().as_str()
    ])
    .to_string();
    if state.probe_identity != probe_identity {
        state.probe_identity = probe_identity;
        ui.set_probe_confirmed(false);
    }
    if state.load_identity != load_identity {
        state.load_identity = load_identity;
        ui.set_load_confirmed(false);
    }
    let fresh = state.settings_target.as_ref() == Some(&state.view.config)
        && state.settings_revision == setup["preferences"]["revision"].as_u64();
    let saved_form = intent(window, state).is_ok_and(|v| v == setup["preferences"]["intent"]);
    ui.set_can_probe(fresh && saved_form && setup["selection"]["probe"]["id"].is_string());
    ui.set_can_load(fresh && saved_form && setup["prepared"]["plan"]["id"].is_string());
    ui.set_can_cancel(running && native["operation"]["kind"] != "unload");
    ui.set_can_unload(native["runtime"]["phase"] == "ready" && !running);
}
