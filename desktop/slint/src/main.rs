use slint::ComponentHandle;
use smith_native_bridge::{
    fixture::Fixture,
    native::{register_reviewed_fonts, Driver, MainWindow, NativeMlx},
    owned_backend::OwnedBackend,
};
use std::{cell::Cell, path::PathBuf, rc::Rc, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let fixture = args == ["--fixture"] || args == ["--fixture", "--bounded"];
    let bounded_package =
        args.len() == 3 && args[0] == "--profile" && args[2] == "--qualification-bounded";
    let selected = if fixture {
        None
    } else if args.is_empty() {
        let backend = std::env::current_exe()?
            .parent()
            .ok_or("Application bundle path unavailable.")?
            .join("../Resources/smith-next");
        let profile = PathBuf::from(
            std::env::var_os("HOME").ok_or("Local application support directory unavailable.")?,
        )
        .join("Library/Application Support/cloud.lazarev.SmithNext");
        std::fs::create_dir_all(&profile)?;
        Some((backend, profile))
    } else if bounded_package {
        let backend = std::env::current_exe()?
            .parent()
            .ok_or("Application bundle path unavailable.")?
            .join("../Resources/smith-next");
        Some((backend, PathBuf::from(&args[1])))
    } else if args.len() == 4 && args[0] == "--backend" && args[2] == "--profile" {
        Some((PathBuf::from(&args[1]), PathBuf::from(&args[3])))
    } else {
        return Err("Use the application bundle, --backend PATH --profile DIRECTORY for explicit qualification, or --fixture [--bounded]. Existing server URLs are never accepted.".into());
    };
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    register_reviewed_fonts()?;
    let window = MainWindow::new()?;
    if !fixture {
        // Fixed app-bundle resource path only: no directory search or execution.
        let worker = std::env::current_exe()?
            .parent()
            .ok_or("Application path unavailable.")?
            .join("../Resources/MLX/smith-mlx-worker");
        window
            .global::<NativeMlx>()
            .set_worker(worker.to_string_lossy().into_owned().into());
        window
            .global::<NativeMlx>()
            .set_source("Bundled Smith native MLX worker".into());
        window
            .global::<NativeMlx>()
            .set_source_revision(env!("CARGO_PKG_VERSION").into());
    }
    let profile_root = selected.as_ref().map(|(_, profile)| profile.clone());
    let backend = selected
        .map(|(backend, profile)| OwnedBackend::launch(&backend, &profile))
        .transpose()?;
    let driver = Rc::new(if let Some(backend) = &backend {
        Driver::attach_transport_with_profile(
            &window,
            backend.transport(),
            profile_root
                .as_deref()
                .ok_or("Owned profile path unavailable.")?,
        )?
    } else {
        Driver::attach(&window, Fixture::start()?)
    });
    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let tick = driver.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(25),
        move || {
            if let Some(window) = weak.upgrade() {
                tick.pump(&window);
            }
        },
    );
    let qualified = Rc::new(Cell::new(false));
    if args == ["--fixture", "--bounded"] || bounded_package {
        let ready = qualified.clone();
        let weak = Rc::downgrade(&driver);
        slint::Timer::single_shot(Duration::from_secs(5), move || {
            ready.set(weak.upgrade().is_some_and(|driver| driver.ready()));
            let _ = slint::quit_event_loop();
        });
    }
    let result = window.run();
    timer.stop();
    drop(timer);
    let draft_save = driver.finish(&window);
    drop(driver);
    let shutdown = backend.as_ref().map(OwnedBackend::stop).transpose();
    result?;
    draft_save?;
    shutdown?;
    if bounded_package && !qualified.get() {
        return Err(
            "Bundled native workspace did not become ready before qualification ended.".into(),
        );
    }
    if bounded_package {
        println!("PASS: bundled native workspace ready; owned backend stopped cleanly.");
    }
    Ok(())
}
