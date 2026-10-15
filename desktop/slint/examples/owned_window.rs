//! Bounded actual native window + actual owned normal backend, synthetic data.
//! Only this explicitly invoked example drives callbacks and takes a snapshot.
#[cfg(feature = "native-ui")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use slint::{ComponentHandle, Model};
    use smith_native_bridge::{
        native::{register_reviewed_fonts, Driver, MainWindow, Palette},
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
    let executable = PathBuf::from(
        std::env::var_os("SMITH_NATIVE_TEST_BACKEND")
            .ok_or("Set the exact reviewed normal backend for qualification.")?,
    );
    let root = std::env::temp_dir().join(format!(
        "smith-native-window-{}-{}",
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
    let backend = OwnedBackend::launch(&executable, &root)?;
    let window = MainWindow::new()?;
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 800));
    let driver = Rc::new(Driver::attach_transport(&window, backend.transport()));
    let stage = Rc::new(Cell::new(0));
    let failure = Rc::new(RefCell::new(None::<String>));
    let timer = slint::Timer::default();
    let tick = driver.clone();
    let weak = window.as_weak();
    let progress = stage.clone();
    let error = failure.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(25),
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            tick.pump(&window);
            if Instant::now() > deadline {
                *error.borrow_mut() = Some(format!(
                    "Native workflow timed out: {}",
                    window.get_status_text()
                ));
                let _ = slint::quit_event_loop();
                return;
            }
            match progress.get() {
                0 if tick.ready() => {
                    assert!(!window.get_show_welcome());
                    assert!(!window.global::<Palette>().get_dark());
                    window.invoke_new_session();
                    progress.set(1);
                }
                1 if tick.view().conversation.is_some() => {
                    window.set_composer_text("Owned native window qualification".into());
                    window.invoke_send("Owned native window qualification".into());
                    window.set_composer_text("Unsent native draft".into());
                    progress.set(2);
                }
                2 if window.get_streaming() => progress.set(3),
                3 if !window.get_streaming() && window.get_messages().row_count() == 2 => {
                    let result = (|| -> Result<(), String> {
                        if window.get_composer_text() != "Unsent native draft" {
                            return Err("Pending request erased the later draft.".into());
                        }
                        let view = tick.view();
                        let answer = view
                            .conversation
                            .as_ref()
                            .unwrap()
                            .active_messages
                            .last()
                            .unwrap();
                        if answer.status != "completed" || answer.content.is_empty() {
                            return Err(
                                "Native view did not receive the completed owned backend answer."
                                    .into(),
                            );
                        }
                        if let Some(path) = std::env::var_os("SMITH_NATIVE_FIXTURE_SNAPSHOT") {
                            let pixels =
                                window.window().take_snapshot().map_err(|e| e.to_string())?;
                            let mut bytes =
                                format!("P6\n{} {}\n255\n", pixels.width(), pixels.height())
                                    .into_bytes();
                            for p in pixels.as_slice() {
                                bytes.extend_from_slice(&[p.r, p.g, p.b]);
                            }
                            std::fs::write(path, bytes).map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    })();
                    if let Err(message) = result {
                        *error.borrow_mut() = Some(message);
                    } else {
                        progress.set(4);
                    }
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        },
    );
    let result = window.run();
    timer.stop();
    drop(timer);
    let draft_save = driver.finish(&window);
    drop(driver);
    let stopped = backend.stop();
    result?;
    draft_save?;
    stopped?;
    if let Some(error) = failure.borrow_mut().take() {
        return Err(error.into());
    }
    if stage.get() != 4 {
        return Err("Native workflow did not finish.".into());
    }
    drop(backend);
    drop(temporary);
    println!("PASS: actual original Slint window + owned normal backend; automatic startup, New chat, Send, answer snapshots, later draft preserved, light appearance, clean child shutdown.");
    Ok(())
}
#[cfg(not(feature = "native-ui"))]
fn main() {
    panic!("Build this explicit qualification example with --features native-ui");
}
