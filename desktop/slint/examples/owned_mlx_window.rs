//! Opt-in owned window + approved tiny synthetic Metal package. No discovery,
//! downloaded weights, OS capture/input or ordinary user's profile is involved.
#[cfg(feature = "native-ui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use slint::ComponentHandle;
    use smith_native_bridge::{
        native::{register_reviewed_fonts, Driver, MainWindow, NativeMlx, NativeSettings},
        owned_backend::OwnedBackend,
    };
    use std::{
        cell::{Cell, RefCell},
        path::PathBuf,
        rc::Rc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let backend_path = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND").ok_or("Explicit frozen backend required")?,
    );
    let worker = std::env::var("SMITH_NATIVE_MLX_WORKER")?;
    let model = std::env::var("SMITH_NATIVE_MLX_MODEL")?;
    let root = std::env::temp_dir().join(format!(
        "smith-native-metal-window-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    std::fs::create_dir(&root)?;
    let temporary = Temp(root.clone());
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    register_reviewed_fonts()?;
    let backend = OwnedBackend::launch(&backend_path, &root)?;
    let w = MainWindow::new()?;
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    let d = Rc::new(Driver::attach_transport(&w, backend.transport()));
    let stage = Rc::new(Cell::new(0));
    let failure = Rc::new(RefCell::new(None::<String>));
    let timer = slint::Timer::default();
    let weak = w.as_weak();
    let driver = d.clone();
    let progress = stage.clone();
    let error = failure.clone();
    let deadline = Instant::now() + Duration::from_secs(150);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(25),
        move || {
            let Some(w) = weak.upgrade() else { return };
            driver.pump(&w);
            let ui = w.global::<NativeMlx>();
            let view = driver.view();
            let result = (|| -> Result<(), String> {
                if Instant::now() > deadline {
                    return Err(format!(
                        "Native MLX stage {} timed out: {}",
                        progress.get(),
                        w.get_status_text()
                    ));
                }
                match progress.get() {
                    0 if driver.ready() => {
                        w.set_active_view(2);
                        w.set_settings_tab(3);
                        ui.invoke_refresh();
                        progress.set(1);
                    }
                    1 if ui.get_available()
                        && !ui.get_busy()
                        && w.global::<NativeSettings>().get_available()
                        && !w.global::<NativeSettings>().get_busy() =>
                    {
                        w.global::<NativeSettings>().set_engine_index(1);
                        ui.set_worker(worker.clone().into());
                        ui.set_directory(model.clone().into());
                        ui.set_model_id("synthetic-constant-a".into());
                        ui.set_source("locally built Swift worker with owned resources".into());
                        ui.set_source_revision("synthetic-native-window-v1".into());
                        ui.set_capability_index(0);
                        ui.set_max_tokens("16".into());
                        ui.set_temperature("0.0".into());
                        ui.set_top_p("1.0".into());
                        ui.set_seed("0".into());
                        ui.invoke_prepare();
                        progress.set(2);
                    }
                    2 if ui.get_can_load() && !ui.get_busy() => {
                        snapshot(&w, "smith-native-metal-inference.ppm")?;
                        ui.set_confirmed(true);
                        ui.invoke_load();
                        progress.set(3);
                    }
                    3 if !view.mlx["runtime"].is_null()
                        && view.config.provider == "mlx"
                        && !ui.get_busy() =>
                    {
                        w.invoke_new_session();
                        progress.set(4);
                    }
                    4 if view.conversation.is_some() && !w.get_session_loading() => {
                        w.set_composer_text("Synthetic native Metal qualification".into());
                        w.invoke_send("Synthetic native Metal qualification".into());
                        progress.set(5);
                    }
                    5 if view
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.active_messages.len() == 2)
                        && !w.get_streaming() =>
                    {
                        let answer = view
                            .conversation
                            .as_ref()
                            .unwrap()
                            .active_messages
                            .last()
                            .unwrap();
                        if answer.content != "A".repeat(16) || answer.status != "limited" {
                            return Err(format!(
                                "Unexpected tiny-package completion: status={},length={}",
                                answer.status,
                                answer.content.len()
                            ));
                        }
                        snapshot(&w, "smith-native-metal-chat.ppm")?;
                        ui.invoke_unload();
                        progress.set(6);
                    }
                    6 if view.mlx["runtime"].is_null() && !ui.get_busy() => {
                        ui.invoke_refresh();
                        progress.set(7);
                    }
                    7 if !ui.get_busy() => {
                        ui.set_capability_index(1);
                        ui.set_max_tokens("2048".into());
                        ui.set_temperature("0.0".into());
                        ui.set_top_p("1.0".into());
                        ui.set_seed("0".into());
                        ui.invoke_prepare();
                        progress.set(8);
                    }
                    8 if ui.get_can_load() && !ui.get_busy() => {
                        ui.set_confirmed(true);
                        ui.invoke_load();
                        progress.set(9);
                    }
                    9 if ui.get_raw_ready() && !ui.get_busy() => {
                        ui.set_prompt("Synthetic cancellation proof".into());
                        ui.invoke_complete();
                        progress.set(10);
                    }
                    10 if view.mlx["operation"]["status"] == "running"
                        && view.mlx["operation"]["progress"]["text"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty()) =>
                    {
                        ui.invoke_cancel();
                        progress.set(11);
                    }
                    11 if view.mlx["operation"]["status"] == "cancelled" && !ui.get_busy() => {
                        let runtime = &view.mlx["runtime"];
                        if !runtime.is_null()
                            && (runtime["pid"].is_number()
                                || runtime["spawned"] != runtime["reaped"])
                        {
                            return Err(
                                "Native MLX cancellation did not confirm child reap.".into()
                            );
                        }
                        w.set_active_view(2);
                        w.set_settings_tab(3);
                        w.global::<NativeSettings>().set_engine_index(1);
                        snapshot(&w, "smith-native-metal-cancel.ppm")?;
                        progress.set(12);
                        let _ = slint::quit_event_loop();
                    }
                    _ => (),
                }
                Ok(())
            })();
            if let Err(e) = result {
                *error.borrow_mut() = Some(e);
                let _ = slint::quit_event_loop();
            }
        },
    );
    let run = w.run();
    timer.stop();
    drop(timer);
    let draft = d.finish(&w);
    drop(d);
    let shutdown = backend.stop();
    run?;
    draft?;
    shutdown?;
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error.into());
    }
    if stage.get() != 12 {
        return Err("Native MLX workflow did not finish".into());
    }
    drop(backend);
    drop(temporary);
    println!("PASS: native selected-file review, explicit Metal chat load, exact 16-token completion, separate raw load, visible cancellation and owned cleanup.");
    Ok(())
}
#[cfg(feature = "native-ui")]
fn snapshot(w: &smith_native_bridge::native::MainWindow, name: &str) -> Result<(), String> {
    use slint::ComponentHandle;
    let image = w.window().take_snapshot().map_err(|e| e.to_string())?;
    let mut bytes = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
    for p in image.as_slice() {
        bytes.extend_from_slice(&[p.r, p.g, p.b]);
    }
    std::fs::write(std::env::temp_dir().join(name), bytes).map_err(|e| e.to_string())
}
#[cfg(not(feature = "native-ui"))]
fn main() {}
