use super::*;
use crate::{
    mlx::{MlxReview, MlxSelection, ThinkingMode},
    settings::SelectedFile,
};
use serde_json::{json, Value};
pub(super) enum Action {
    Refresh,
    Prepare(MlxReview, MlxSelection),
    Load(MlxReview),
    Cancel(Config, Value),
    Unload(Config, Value),
    Raw(Config, Value, String, Value),
}
impl Action {
    pub(super) fn execute<T: Transport>(self, b: &mut Bridge<T>) -> crate::bridge::Result<()> {
        match self {
            Self::Refresh => b.refresh_mlx(),
            Self::Prepare(r, s) => b.prepare_mlx(&r, s),
            Self::Load(r) => b.load_mlx(&r),
            Self::Cancel(c, o) => b.cancel_mlx(&c, &o),
            Self::Unload(c, r) => b.unload_mlx(&c, &r),
            Self::Raw(c, r, p, s) => b.raw_mlx(&c, &r, p, s),
        }
    }
}
pub(super) fn thinking(w: &MainWindow) -> ThinkingMode {
    if w.global::<NativeMlx>().get_thinking_index() == 1 {
        ThinkingMode::Disabled
    } else {
        ThinkingMode::ModelDefault
    }
}
pub(super) fn sampling(window: &MainWindow) -> crate::bridge::Result<Value> {
    let ui = window.global::<NativeMlx>();
    let max = ui
        .get_max_tokens()
        .parse::<u32>()
        .map_err(|_| "Max tokens must be 1–2048.")?;
    let temperature = ui
        .get_temperature()
        .parse::<f32>()
        .map_err(|_| "Temperature must be 0–2.")?;
    let top_p = ui
        .get_top_p()
        .parse::<f32>()
        .map_err(|_| "Top-p must be >0–1.")?;
    if !(1..=2048).contains(&max)
        || !temperature.is_finite()
        || !(0.0..=2.0).contains(&temperature)
        || !top_p.is_finite()
        || top_p <= 0.0
        || top_p > 1.0
    {
        return Err("MLX sampling is outside the supported bounds.".into());
    }
    let mut value = json!({"max_tokens":max,"temperature":temperature,"top_p":top_p});
    if !ui.get_seed().is_empty() {
        value["seed"] = json!(ui
            .get_seed()
            .parse::<u64>()
            .map_err(|_| "Seed must be an unsigned integer.")?);
    }
    Ok(value)
}
fn review(state: &UiState) -> crate::bridge::Result<MlxReview> {
    if state.mlx_target.as_ref() != Some(&state.view.config)
        || state.mlx_revision != state.view.mlx["settings"]["revision"].as_u64()
    {
        return Err(
            "Refresh and review MLX settings after the connection or saved settings change.".into(),
        );
    }
    Ok(MlxReview {
        config: state.view.config.clone(),
        settings_revision: state.mlx_revision.ok_or("Refresh MLX settings first.")?,
        prepared: state.view.mlx["prepared"].clone(),
    })
}
fn prepared_thinking_matches(window: &MainWindow, s: &UiState) -> bool {
    let value = s.view.mlx["prepared"]
        .get("thinking_mode")
        .cloned()
        .unwrap_or_else(|| json!("model_default"));
    serde_json::from_value::<ThinkingMode>(value).is_ok_and(|mode| mode == thinking(window))
}
fn identity(window: &MainWindow, s: &UiState) -> String {
    let ui = window.global::<NativeMlx>();
    json!([
        s.view.config.revision,
        s.view.mlx["settings"]["revision"],
        s.view.mlx["prepared"],
        ui.get_worker().as_str(),
        ui.get_directory().as_str(),
        ui.get_model_id().as_str(),
        ui.get_source().as_str(),
        ui.get_source_revision().as_str(),
        ui.get_capability_index(),
        ui.get_thinking_index(),
        ui.get_max_tokens().as_str(),
        ui.get_temperature().as_str(),
        ui.get_top_p().as_str(),
        ui.get_seed().as_str()
    ])
    .to_string()
}
pub(super) fn wire(
    window: &MainWindow,
    state: &Rc<RefCell<UiState>>,
    tx: &mpsc::Sender<(u64, Command)>,
) {
    let register = |f: fn(&MainWindow, &UiState) -> crate::bridge::Result<Action>| {
        let weak = window.as_weak();
        let state = state.clone();
        let tx = tx.clone();
        move || {
            let Some(w) = weak.upgrade() else { return };
            let mut s = state.borrow_mut();
            if s.mlx_pending || !s.view.available {
                return;
            }
            if !crate::platform_ui::Platform::current().mlx() {
                s.last_error = Some("Native MLX requires the Apple silicon macOS build.".into());
                return;
            }
            match f(&w, &s) {
                Ok(a) => {
                    s.mlx_pending = true;
                    let _ = tx.send((s.epoch.current(), Command::Mlx(a)));
                }
                Err(e) => s.last_error = Some(e),
            }
        }
    };
    let ui = window.global::<NativeMlx>();
    ui.on_refresh(register(|_, _| Ok(Action::Refresh)));
    ui.on_prepare(register(|w, s| {
        let r = review(s)?;
        let ui = w.global::<NativeMlx>();
        Ok(Action::Prepare(
            r,
            MlxSelection {
                worker: SelectedFile {
                    path: ui.get_worker().into(),
                    source: ui.get_source().into(),
                    source_revision: ui.get_source_revision().into(),
                },
                directory: ui.get_directory().into(),
                model_id: ui.get_model_id().into(),
                capability: if ui.get_capability_index() == 1 {
                    "raw_completion"
                } else {
                    "chat"
                }
                .into(),
                sampling: sampling(w)?,
                thinking_mode: thinking(w),
            },
        ))
    }));
    ui.on_load(register(|w, s| {
        if !w.global::<NativeMlx>().get_confirmed()
            || s.mlx_identity != identity(w, s)
            || !prepared_thinking_matches(w, s)
        {
            return Err("Review and confirm this exact MLX plan first.".into());
        }
        Ok(Action::Load(review(s)?))
    }));
    ui.on_cancel(register(|_, s| {
        Ok(Action::Cancel(
            s.view.config.clone(),
            s.view.mlx["operation"].clone(),
        ))
    }));
    ui.on_unload(register(|_, s| {
        Ok(Action::Unload(
            s.view.config.clone(),
            s.view.mlx["runtime"].clone(),
        ))
    }));
    ui.on_complete(register(|w, s| {
        Ok(Action::Raw(
            s.view.config.clone(),
            s.view.mlx["runtime"].clone(),
            w.global::<NativeMlx>().get_prompt().into(),
            sampling(w)?,
        ))
    }));
    for directory in [false, true] {
        let state = state.clone();
        let weak = window.as_weak();
        let handler = move || {
            let epoch = {
                let s = state.borrow();
                if !s.view.available || s.mlx_pending {
                    return;
                }
                s.epoch.current()
            };
            let result = crate::picker::choose(
                if directory {
                    "Choose an existing safetensors package. No files are inspected until you request inspection."
                } else {
                    "Choose the existing native MLX worker. Selection does not execute it."
                },
                directory,
            );
            if !state.borrow().epoch.accepts(epoch) {
                return;
            }
            match result {
                Ok(Some(path)) => {
                    if let Some(w) = weak.upgrade() {
                        if directory {
                            w.global::<NativeMlx>().set_directory(path.into())
                        } else {
                            w.global::<NativeMlx>().set_worker(path.into())
                        }
                    }
                }
                Ok(None) => (),
                Err(e) => state.borrow_mut().last_error = Some(e),
            }
        };
        if directory {
            ui.on_pick_directory(handler);
        } else {
            ui.on_pick_worker(handler);
        }
    }
}
pub(super) fn populate(window: &MainWindow, s: &mut UiState) {
    s.mlx_target = Some(s.view.config.clone());
    s.mlx_revision = s.view.mlx["settings"]["revision"].as_u64();
    let ui = window.global::<NativeMlx>();
    ui.set_thinking_index(if s.view.mlx["settings"]["thinking_mode"] == "disabled" {
        1
    } else {
        0
    });
    let sampling = &s.view.mlx["settings"]["sampling"];
    ui.set_max_tokens(sampling["max_tokens"].to_string().into());
    ui.set_temperature(crate::mlx::sampling_value_text(&sampling["temperature"]).into());
    ui.set_top_p(crate::mlx::sampling_value_text(&sampling["top_p"]).into());
    ui.set_seed(
        sampling["seed"]
            .as_u64()
            .map(|v| v.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_capability_index(
        if s.view.mlx["settings"]["capability"] == "raw_completion" {
            1
        } else {
            0
        },
    );
}
pub(super) fn project(window: &MainWindow, s: &mut UiState) {
    let ui = window.global::<NativeMlx>();
    let value = &s.view.mlx;
    let running = value["operation"]["status"] == "running";
    ui.set_thinking_supported(crate::mlx::supports_thinking_control(value));
    ui.set_available(
        crate::platform_ui::Platform::current().mlx() && s.view.available && !value.is_null(),
    );
    ui.set_busy(s.mlx_pending || running);
    ui.set_can_cancel(running && value["operation"]["kind"] != "unload");
    ui.set_can_unload(!value["runtime"].is_null() && !running);
    ui.set_raw_ready(
        value["loaded"]["capability"] == "raw_completion" && !value["runtime"].is_null(),
    );
    ui.set_status(if value.is_null() {
        "Refresh MLX settings to begin. No worker is loaded automatically.".into()
    } else {
        format!(
            "{}\n{} · {}{}\n{}",
            value["loaded"]["model_id"]
                .as_str()
                .unwrap_or("No owned MLX model loaded"),
            value["operation"]["kind"].as_str().unwrap_or("Idle"),
            value["operation"]["status"].as_str().unwrap_or(""),
            value["operation"]["error"]
                .as_str()
                .map(|e| format!(" · {e}"))
                .unwrap_or_default(),
            if value["runtime"].is_null() {
                String::new()
            } else {
                format!(
                    "Owned worker: {} · {}",
                    value["runtime"]["phase"].as_str().unwrap_or("unknown"),
                    value["runtime"]["backend"].as_str().unwrap_or("unknown")
                )
            }
        )
        .into()
    });
    let p = &value["prepared"];
    ui.set_review(
        if p.is_null() {
            String::new()
        } else {
            format!(
                "Worker: {}\nModel: {}\n{} · output {} · temperature {} · top-p {}\nThinking: {}\nLoads the selected package with this worker.",
                p["plan"]["worker"]["path"].as_str().unwrap_or(""),
                p["plan"]["model"]["root"].as_str().unwrap_or("selected directory"),
                p["capability"].as_str().unwrap_or("").replace('_', " "),
                p["sampling"]["max_tokens"],
                crate::mlx::sampling_value_text(&p["sampling"]["temperature"]),
                crate::mlx::sampling_value_text(&p["sampling"]["top_p"]),
                if p["thinking_mode"] == "disabled" { "Disabled" } else { "Model default" },
            )
        }.into(),
    );
    ui.set_detail(
        if p.is_null() {
            String::new()
        } else {
            serde_json::to_string_pretty(p).unwrap_or_default()
        }
        .into(),
    );
    ui.set_result(
        value["operation"]["completion"]["text"]
            .as_str()
            .or_else(|| value["operation"]["progress"]["text"].as_str())
            .unwrap_or("")
            .into(),
    );
    let id = identity(window, s);
    if s.mlx_identity != id {
        s.mlx_identity = id;
        ui.set_confirmed(false);
    }
    ui.set_can_load(
        review(s).is_ok()
            && value["prepared"]["plan"]["id"].is_string()
            && prepared_thinking_matches(window, s),
    );
}
pub(super) fn clear(window: &MainWindow, s: &mut UiState) {
    s.mlx_target = None;
    s.mlx_revision = None;
    s.mlx_identity.clear();
    s.mlx_pending = false;
    let ui = window.global::<NativeMlx>();
    ui.set_worker("".into());
    ui.set_directory("".into());
    ui.set_model_id("".into());
    ui.set_source("".into());
    ui.set_source_revision("".into());
    ui.set_prompt("".into());
    ui.set_result("".into());
    ui.set_confirmed(false);
}
