#![cfg(feature = "native-ui")]
use slint::{ComponentHandle, Model};
use smith_native_bridge::{
    native::{register_reviewed_fonts, Driver, MainWindow, NativePlatform, NativeSettings},
    platform_ui::Platform,
};
#[path = "support/settings_fixture.rs"]
mod settings_fixture;
use std::{
    thread,
    time::{Duration, Instant},
};
fn wait(w: &MainWindow, d: &Driver, test: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        d.pump(w);
        if test() {
            return;
        }
        assert!(Instant::now() < end, "{}", w.get_status_text());
        thread::sleep(Duration::from_millis(10));
    }
}
fn snapshot(w: &MainWindow, name: &str) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    let pixels = w.window().take_snapshot().unwrap();
    if let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") {
        std::fs::create_dir_all(&root).unwrap();
        let mut data = format!("P6\n{} {}\n255\n", pixels.width(), pixels.height()).into_bytes();
        for p in pixels.as_slice() {
            data.extend_from_slice(&[p.r, p.g, p.b]);
        }
        std::fs::write(
            std::path::Path::new(&root).join(format!("{name}.ppm")),
            data,
        )
        .unwrap();
    }
}
#[test]
fn actual_native_settings_resolve_platform_ids_without_overwriting_foreign_preferences() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            threading: false,
            ..Default::default()
        },
    )))
    .unwrap();
    register_reviewed_fonts().unwrap();
    let fixture = settings_fixture::SettingsFixture::new();
    let control = fixture.control.clone();
    let platform = Platform::current();
    let foreign = if platform.os == "macos" {
        "llama_cuda"
    } else {
        "llama_metal"
    };
    control.0.lock().unwrap().setup["preferences"]["intent"]["overrides"]["backend"] =
        serde_json::json!(foreign);
    let w = MainWindow::new().unwrap();
    let d = Driver::attach_transport(&w, fixture);
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.set_active_view(2);
    let settings = w.global::<NativeSettings>();
    wait(&w, &d, || settings.get_available());
    assert_eq!(
        settings
            .get_backend_choices()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>(),
        platform
            .backends()
            .iter()
            .map(|v| v.label.to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        settings
            .get_provider_choices()
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>(),
        platform
            .providers()
            .iter()
            .map(|v| v.label.to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(w.global::<NativePlatform>().get_mlx(), platform.mlx());
    assert_eq!(
        w.global::<NativePlatform>().get_pickers(),
        platform.pickers()
    );
    assert_eq!(settings.get_backend_index(), -1);
    assert!(settings.get_unavailable_backend().contains(foreign));
    let before = control.0.lock().unwrap().calls.len();
    settings.invoke_preferences();
    for _ in 0..20 {
        d.pump(&w);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(control.0.lock().unwrap().calls.len(), before);
    assert_eq!(
        control.0.lock().unwrap().setup["preferences"]["intent"]["overrides"]["backend"],
        foreign
    );
    settings.set_backend_index(1);
    settings.invoke_preferences();
    wait(&w, &d, || {
        control.0.lock().unwrap().setup["preferences"]["intent"]["overrides"]["backend"]
            == "llama_cpu"
    });
    wait(&w, &d, || !settings.get_busy());
    assert!(
        settings.get_unavailable_backend().is_empty(),
        "Successful saved replacement must clear the incompatible-backend warning"
    );
    assert_eq!(control.0.lock().unwrap().loads, 0);
    assert_eq!(control.0.lock().unwrap().discoveries, 0);
    w.set_status_text("".into());
    snapshot(&w, "platform-general-settings");
    w.set_settings_tab(1);
    snapshot(&w, "platform-server-settings");
    w.set_settings_tab(3);
    settings.set_engine_index(0);
    snapshot(&w, "platform-gguf-settings");
    if platform.mlx() {
        settings.set_engine_index(1);
        snapshot(&w, "platform-mlx-settings");
    }
    drop(d);
    w.hide().unwrap();
}
