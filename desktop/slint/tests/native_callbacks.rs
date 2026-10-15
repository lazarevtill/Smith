#![cfg(feature = "native-ui")]
use slint::{ComponentHandle, Model};
#[path = "support/settings_fixture.rs"]
mod settings_fixture;
use smith_native_bridge::{
    fixture::Fixture,
    native::{
        register_reviewed_fonts, Driver, MainWindow, NativeCatalog, NativeMlx, NativeSettings,
        Palette,
    },
};
use std::{
    thread,
    time::{Duration, Instant},
};

fn wait(window: &MainWindow, driver: &Driver, condition: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        driver.pump(window);
        if condition() {
            return;
        }
        assert!(
            Instant::now() < until,
            "native callback did not reach its expected state: {}",
            window.get_status_text()
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn capture(window: &MainWindow, name: &str) {
    let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") else {
        return;
    };
    std::fs::create_dir_all(&root).unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    let p = window.window().take_snapshot().unwrap();
    let mut bytes = format!("P6\n{} {}\n255\n", p.width(), p.height()).into_bytes();
    for pixel in p.as_slice() {
        bytes.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
    }
    std::fs::write(
        std::path::Path::new(&root).join(format!("{name}.ppm")),
        bytes,
    )
    .unwrap();
}
fn raw_toggle_keys(window: &MainWindow) {
    // Locate the one unchecked 18px SmithCheck in this owned, fresh chat's
    // answer column. Search the actual renderer, not the OS or debug metadata.
    let before = window.window().take_snapshot().unwrap();
    let border = |x: usize, y: usize| {
        let p = before.as_slice()[y * before.width() as usize + x];
        p.r.abs_diff(91) < 8 && p.g.abs_diff(91) < 8 && p.b.abs_diff(99) < 8
    };
    let mut boxes = Vec::new();
    for y in 160..500 {
        for x in 430..460 {
            if (5..13).all(|n| border(x + n, y) && border(x + n, y + 17))
                && (5..13).all(|n| border(x, y + n) && border(x + 17, y + n))
            {
                boxes.push((x, y));
            }
        }
    }
    assert_eq!(
        boxes.len(),
        1,
        "expected the actual Show raw response checkbox: {boxes:?}"
    );
    let (x, y) = boxes[0];
    let position = slint::LogicalPosition::new((x + 9) as f32, (y + 9) as f32);
    let button = slint::platform::PointerEventButton::Left;
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    let on = window.window().take_snapshot().unwrap();
    let checked = |p: &slint::SharedPixelBuffer<slint::Rgba8Pixel>| {
        let p = p.as_slice()[(y + 8) * p.width() as usize + x + 3];
        p.r > 180 && p.g < 80 && p.b < 100
    };
    assert!(
        checked(&on),
        "pointer establishes focus on the real raw checkbox"
    );
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    let off = window.window().take_snapshot().unwrap();
    assert!(!checked(&off), "Space toggles the dynamic raw binding off");
    let changed = (y + 24..(y + 220).min(690))
        .flat_map(|row| (430..1170).map(move |col| row * 1200 + col))
        .filter(|&i| on.as_slice()[i] != off.as_slice()[i])
        .count();
    assert!(
        changed > 500,
        "raw text editor must change rendered answer, got {changed} pixels"
    );
    capture(window, "reasoning-keyboard-derived");
    let enter: slint::SharedString = slint::platform::Key::Return.into();
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: enter.clone(),
        });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: enter });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    assert!(
        checked(&window.window().take_snapshot().unwrap()),
        "Return restores raw view"
    );
    capture(window, "reasoning-keyboard-raw");
}
#[test]
fn original_window_callbacks_drive_owned_http_and_clear_revoked_content() {
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
    let fixture = Fixture::start().unwrap();
    let window = MainWindow::new().unwrap();
    let driver = Driver::attach(&window, fixture.clone());
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 800));
    window.show().unwrap();
    wait(&window, &driver, || driver.ready());
    assert!(!window.global::<Palette>().get_dark());
    assert!(!window.get_show_welcome());
    assert_eq!(window.get_models().row_count(), 1);
    assert_eq!(window.get_artifacts_list().row_count(), 1);
    assert_eq!(window.get_activity_tasks().row_count(), 1);
    window.invoke_new_session();
    wait(&window, &driver, || driver.view().conversation.is_some());
    let original_id = driver.view().conversation.unwrap().chat.id;
    window.set_composer_text("Native callback prompt".into());
    // Dispatch only to this owned software window, through the real Slint
    // pointer handler. No OS input/capture or optional debug metadata is used.
    let _ = window.window().take_snapshot().unwrap();
    let position = slint::LogicalPosition::new(1150.0, 754.0);
    let button = slint::platform::PointerEventButton::Left;
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    assert_eq!(window.get_composer_text(), "Native callback prompt");
    window.set_composer_text("A draft typed while the request is pending".into());
    wait(&window, &driver, || window.get_streaming());
    assert_eq!(
        window.get_composer_text(),
        "A draft typed while the request is pending"
    );
    wait(&window, &driver, || !window.get_streaming());
    assert_eq!(window.get_messages().row_count(), 2);
    wait(&window, &driver, || !driver.view().draft.dirty);
    let saved_draft = window.get_composer_text().to_string();
    let user = driver.view().conversation.as_ref().unwrap().active_messages[0].clone();
    window.invoke_edit_message(user.content.into(), user.id.into());
    assert!(window.get_editing_message());
    window.set_composer_text("Edited native user turn".into());
    window.invoke_send("Edited native user turn".into());
    wait(&window, &driver, || window.get_streaming());
    wait(&window, &driver, || !window.get_streaming());
    assert!(!window.get_editing_message());
    assert_eq!(window.get_composer_text(), saved_draft);
    assert_eq!(
        driver.view().conversation.as_ref().unwrap().messages.len(),
        4
    );
    let index = window.get_selected_session();
    window.invoke_rename_session(index, "Native renamed title".into());
    wait(&window, &driver, || {
        driver.view().conversation.as_ref().unwrap().chat.title == "Native renamed title"
    });
    window.invoke_native_undo();
    wait(&window, &driver, || {
        driver.view().conversation.as_ref().unwrap().chat.title != "Native renamed title"
    });
    window.invoke_native_redo();
    wait(&window, &driver, || {
        driver.view().conversation.as_ref().unwrap().chat.title == "Native renamed title"
    });
    window.invoke_delete_session(window.get_selected_session());
    wait(&window, &driver, || driver.view().conversation.is_none());
    window.invoke_native_toggle_trash();
    wait(&window, &driver, || window.get_sessions().row_count() == 1);
    window.invoke_delete_session(0); // Restore in the explicit Trash view.
    wait(&window, &driver, || driver.view().trash.is_empty());
    window.invoke_native_toggle_trash();
    let index = window
        .get_sessions()
        .iter()
        .position(|s| s.id.as_str() == original_id)
        .unwrap();
    window.invoke_select_session(index as i32);
    wait(&window, &driver, || driver.view().conversation.is_some());
    assert_eq!(window.get_composer_text(), saved_draft);
    let first_answer = driver.view().conversation.unwrap().active_leaf.unwrap();
    window.invoke_regenerate(first_answer.clone().into());
    wait(&window, &driver, || window.get_streaming());
    wait(&window, &driver, || !window.get_streaming());
    let second_answer = driver.view().conversation.unwrap().active_leaf.unwrap();
    assert_ne!(first_answer, second_answer);
    window.invoke_switch_branch(second_answer.clone().into(), -1);
    wait(&window, &driver, || {
        driver
            .view()
            .conversation
            .as_ref()
            .unwrap()
            .active_leaf
            .as_deref()
            == Some(&first_answer)
    });
    window.invoke_switch_branch(first_answer.clone().into(), 1);
    wait(&window, &driver, || {
        driver
            .view()
            .conversation
            .as_ref()
            .unwrap()
            .active_leaf
            .as_deref()
            == Some(&second_answer)
    });
    assert_eq!(driver.view().conversation.unwrap().messages.len(), 5);
    assert_eq!(window.get_messages().row_count(), 2);
    window.invoke_new_session();
    wait(&window, &driver, || {
        driver.view().conversation.as_ref().unwrap().chat.id != original_id
    });
    let index = window
        .get_sessions()
        .iter()
        .position(|s| s.id.as_str() == original_id)
        .unwrap();
    window.invoke_select_session(index as i32);
    wait(&window, &driver, || {
        driver.view().conversation.as_ref().unwrap().chat.id == original_id
    });
    assert_eq!(window.get_messages().row_count(), 2);
    window.invoke_native_back();
    wait(&window, &driver, || {
        driver
            .view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id != original_id)
    });
    window.invoke_native_forward();
    wait(&window, &driver, || {
        driver
            .view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id == original_id)
    });
    window.set_composer_text("cancel this".into());
    window.invoke_send("cancel this".into());
    wait(&window, &driver, || window.get_streaming());
    window.invoke_stop();
    wait(&window, &driver, || !window.get_streaming());
    assert_eq!(
        driver
            .view()
            .conversation
            .unwrap()
            .active_messages
            .last()
            .unwrap()
            .status,
        "cancelled"
    );
    // Derived reasoning changes only the rendered segment model. Raw source
    // remains the exact Copy/export value and the persisted fixture response.
    for (case, (raw, derived, thought)) in [
        ("<think>\n\n</think>\n\nVisible answer.", true, ""),
        (
            "<think>\nConsider the synthetic input.\n</think>\nVisible answer.",
            true,
            "Consider the synthetic input.\n",
        ),
        ("```xml\n<think>\nexample\n</think>\n```", false, ""),
        (
            "<think>\n    literal code example\n</think>\nAnswer",
            false,
            "",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        fixture.set_answer(raw).unwrap();
        let previous = driver.view().conversation.unwrap().chat.id;
        window.invoke_new_session();
        wait(&window, &driver, || {
            driver
                .view()
                .conversation
                .as_ref()
                .is_some_and(|c| c.chat.id != previous)
        });
        window.set_composer_text("Synthetic presentation case".into());
        window.invoke_send(window.get_composer_text());
        wait(&window, &driver, || window.get_streaming());
        wait(&window, &driver, || !window.get_streaming());
        let messages = window.get_messages();
        let shown = messages.row_data(messages.row_count() - 1).unwrap();
        assert_eq!(shown.text.as_str(), raw);
        assert_eq!(shown.reasoning_derived, derived);
        assert_eq!(shown.reasoning.as_str(), thought);
        assert_eq!(
            driver
                .view()
                .conversation
                .unwrap()
                .active_messages
                .last()
                .unwrap()
                .content,
            raw
        );
        if derived {
            assert!(shown.segments.iter().any(|s| s.text == "Visible answer."));
        }
        let _ = window.window().take_snapshot().unwrap();
        capture(&window, &format!("reasoning-case-{case}"));
        if case == 0 {
            raw_toggle_keys(&window);
        }
    }
    window.set_appearance("Dark".into());
    window.global::<Palette>().set_dark(true);
    window.invoke_setting_changed();
    assert!(!window.global::<Palette>().get_dark());
    assert_eq!(window.get_appearance(), "Light");
    assert_eq!(window.get_appearance_modes().row_count(), 1);
    let writes = fixture.mutations();
    window.invoke_voice_input();
    assert_eq!(
        window.get_status_text(),
        "voice input is not available in this app yet."
    );
    assert_eq!(fixture.mutations(), writes);
    let pixels = window.window().take_snapshot().unwrap();
    assert_eq!((pixels.width(), pixels.height()), (1200, 800));
    if let Some(path) = std::env::var_os("SMITH_NATIVE_FIXTURE_SNAPSHOT") {
        let mut bytes = format!("P6\n{} {}\n255\n", pixels.width(), pixels.height()).into_bytes();
        for p in pixels.as_slice() {
            bytes.extend_from_slice(&[p.r, p.g, p.b]);
        }
        std::fs::write(path, bytes).unwrap();
    }
    fixture.rotate_session();
    wait(&window, &driver, || !driver.ready());
    assert_eq!(window.get_messages().row_count(), 0);
    assert_eq!(window.get_sessions().row_count(), 0);
    assert_eq!(window.get_artifacts_list().row_count(), 0);
    assert_eq!(window.get_activity_tasks().row_count(), 0);
    assert!(window.get_composer_text().is_empty());
    drop(driver);
    window.hide().unwrap();

    // Native provider and plan controls use a separate in-memory API fixture;
    // no inference executable, file inspection or external server is used.
    let fixture = settings_fixture::SettingsFixture::new();
    let control = fixture.control.clone();
    let window = MainWindow::new().unwrap();
    let driver = Driver::attach_transport(&window, fixture);
    window
        .window()
        .set_size(slint::PhysicalSize::new(1200, 800));
    window.show().unwrap();
    wait(&window, &driver, || driver.ready());
    window.set_active_view(2);
    wait(&window, &driver, || {
        window.global::<NativeSettings>().get_available()
    });
    if !window.get_status_text().is_empty() {
        let position = slint::LogicalPosition::new(1182.0, 18.0);
        let button = slint::platform::PointerEventButton::Left;
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
        window
            .window()
            .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
    }
    capture(&window, "compact-settings-ready");
    let settings = window.global::<NativeSettings>();
    settings.set_provider_index(2);
    settings.set_endpoint("http://127.0.0.1:18777/v1".into());
    settings.invoke_save();
    wait(&window, &driver, || {
        driver.view().config.provider == "openai" && !settings.get_busy()
    });
    assert_eq!(control.0.lock().unwrap().discoveries, 0);
    settings.set_endpoint("http://127.0.0.1:18778/unsaved".into());
    for _ in 0..35 {
        driver.pump(&window);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(settings.get_endpoint(), "http://127.0.0.1:18778/unsaved");
    settings.invoke_test();
    assert!(window
        .get_status_text()
        .contains("Save the edited connection"));
    assert_eq!(control.0.lock().unwrap().discoveries, 0);
    settings.set_endpoint("http://127.0.0.1:18777/v1".into());
    settings.invoke_test();
    wait(&window, &driver, || {
        driver.view().config.tested && !settings.get_busy()
    });
    assert!(driver.view().config.model.is_empty());
    settings.invoke_select_model("fixture-model".into());
    wait(&window, &driver, || {
        driver.view().config.model == "fixture-model" && !settings.get_busy()
    });
    settings.set_advanced(true);
    settings.set_output("1048577".into());
    let calls = control.0.lock().unwrap().calls.len();
    settings.invoke_preferences();
    driver.pump(&window);
    assert_eq!(
        control.0.lock().unwrap().calls.len(),
        calls,
        "out-of-bound output must not be submitted"
    );
    settings.set_output("256".into());
    settings.set_prefill_threads("2".into());
    settings.set_batch("128".into());
    settings.set_microbatch("32".into());
    settings.set_kv_index(2);
    settings.set_flash_index(2);
    settings.set_mmap_index(1);
    settings.set_mlock_index(0);
    settings.set_device("synthetic-cpu".into());
    settings.set_host_budget("8589934592".into());
    settings.set_device_budget("4294967296".into());
    settings.set_headroom("1073741824".into());
    settings.invoke_preferences();
    wait(&window, &driver, || {
        driver.view().settings.setup["preferences"]["intent"]["overrides"]["output_tokens"] == 256
            && !settings.get_busy()
    });
    let overrides = driver.view().settings.setup["preferences"]["intent"]["overrides"].clone();
    assert_eq!(overrides["flash_attention"], false);
    assert_eq!(overrides["mmap"], true);
    assert!(overrides["mlock"].is_null());
    assert_eq!(overrides["batch_tokens"], 128);
    assert_eq!(overrides["microbatch_tokens"], 32);
    assert_eq!(overrides["device_budget_bytes"], 4294967296_u64);
    settings.set_advanced(false);
    settings.set_executable("/synthetic/runtime".into());
    settings.set_model("/synthetic/model.gguf".into());
    settings.set_model_id("Synthetic".into());
    settings.set_source("fixture only".into());
    settings.set_source_revision("v1".into());
    settings.invoke_inspect();
    wait(&window, &driver, || {
        settings.get_can_probe() && !settings.get_busy()
    });
    settings.set_probe_confirmed(true);
    control.0.lock().unwrap().setup["selection"]["id"] = serde_json::json!("other-tab-selection");
    wait(&window, &driver, || !settings.get_probe_confirmed());
    assert_eq!(control.0.lock().unwrap().loads, 0);
    settings.set_probe_confirmed(true);
    settings.invoke_probe();
    wait(&window, &driver, || {
        settings.get_can_load() && !settings.get_busy()
    });
    settings.set_load_confirmed(true);
    control.0.lock().unwrap().setup["prepared"]["plan"]["id"] = serde_json::json!("other-tab-plan");
    wait(&window, &driver, || !settings.get_load_confirmed());
    settings.set_load_confirmed(true);
    settings.invoke_apply();
    wait(&window, &driver, || {
        settings.get_can_unload() && !settings.get_busy()
    });
    assert_eq!(control.0.lock().unwrap().loads, 1);
    settings.invoke_unload();
    wait(&window, &driver, || {
        !settings.get_can_unload() && !settings.get_busy()
    });
    // The actual native MLX controls capture thinking mode and clear approval
    // when either the visible input or exact prepared review changes. This is
    // an owned synthetic transport, with no worker/model execution.
    window.set_active_view(2);
    window.set_settings_tab(3);
    settings.set_engine_index(1);
    let mlx = window.global::<NativeMlx>();
    mlx.invoke_refresh();
    wait(&window, &driver, || mlx.get_available() && !mlx.get_busy());
    assert!(mlx.get_thinking_supported());
    mlx.set_worker("/synthetic/selected-worker".into());
    mlx.set_directory("/synthetic/selected-package".into());
    mlx.set_model_id("Synthetic thinking review".into());
    mlx.set_thinking_index(1);
    mlx.invoke_prepare();
    wait(&window, &driver, || mlx.get_can_load() && !mlx.get_busy());
    assert!(mlx.get_review().contains("Thinking: Disabled"));
    assert!(mlx.get_review().contains("temperature 0.6 · top-p 0.9"));
    assert!(!mlx.get_review().contains("0.600000023"));
    capture(&window, "native-thinking-review");
    assert_eq!(
        control.0.lock().unwrap().mlx["prepared"]["thinking_mode"],
        "disabled"
    );
    mlx.set_confirmed(true);
    mlx.set_thinking_index(0);
    driver.pump(&window);
    assert!(!mlx.get_confirmed());
    assert!(!mlx.get_can_load());
    // Re-checking cannot make an edited mode approve the prior plan.
    mlx.set_confirmed(true);
    mlx.invoke_load();
    driver.pump(&window);
    assert!(!control
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|(p, _)| p == "/api/mlx/load"));
    mlx.set_thinking_index(1);
    driver.pump(&window);
    mlx.set_confirmed(true);
    control.0.lock().unwrap().mlx["prepared"]["review_id"] = serde_json::json!("b".repeat(64));
    wait(&window, &driver, || !mlx.get_confirmed());
    assert!(!control
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .any(|(p, _)| p == "/api/mlx/load"));
    mlx.set_confirmed(true);
    mlx.invoke_load();
    wait(&window, &driver, || {
        !mlx.get_busy() && driver.view().mlx["loaded"]["thinking_mode"] == "disabled"
    });
    assert_eq!(
        control
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(p, _)| p == "/api/mlx/load")
            .count(),
        1
    );
    assert_eq!(
        control
            .0
            .lock()
            .unwrap()
            .calls
            .iter()
            .find(|(p, _)| p == "/api/mlx/load")
            .unwrap()
            .1["review_id"],
        "b".repeat(64)
    );
    // A saved MLX selection opens the MLX panel even when the current provider
    // remains OpenAI and the previous Inference panel was GGUF.
    {
        let mut s = control.0.lock().unwrap();
        let revision = s.config["revision"].clone();
        s.catalog = serde_json::json!({"catalog":{"revision":2,"sha256":"c".repeat(64)},"entries":[{"id":"local-0000000000000001","revision":1,"label":"Qwen 0.6B · локальная модель","trashed":false,"format":"mlx","declared_capabilities":["text_chat"],"origin":"external_user_selected","completeness":"selected_files_only","selected_for_review":true}],"selected_for_review":"local-0000000000000001","operations":[],"prepared":{"id":"d".repeat(64),"catalog_revision":2,"entry_id":"local-0000000000000001","entry_revision":1,"manifest_sha256":"e".repeat(64),"context":{"session_id":"fixture-session-1","config_revision":revision,"inventory_generation":"synthetic-observation"}},"inspection":"stored_metadata_only","automatic_load":false,"hints_applied":false});
    }
    settings.set_engine_index(0);
    window.set_active_view(1);
    let catalog = window.global::<NativeCatalog>();
    wait(&window, &driver, || {
        catalog.get_prepared() && !catalog.get_busy()
    });
    catalog.invoke_continue_setup();
    wait(&window, &driver, || {
        window.get_active_view() == 2
            && window.get_settings_tab() == 3
            && settings.get_engine_index() == 1
            && !settings.get_busy()
    });
    for _ in 0..35 {
        driver.pump(&window);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        settings.get_engine_index(),
        1,
        "settings refresh must preserve the explicit catalog adapter"
    );
    capture(&window, "native-catalog-mlx-handoff");
    let body = control
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .find(|(p, _)| p == "/api/local-catalog/setup")
        .unwrap()
        .1
        .clone();
    assert_eq!(body["settings"]["model_id"], "local-0000000000000001");
    assert_eq!(
        driver.view().config.provider,
        "openai",
        "handoff never changes active provider"
    );
    // Navigation discards an old screen's view but releases its completed
    // single-flight reservation, so returning does not leave controls disabled.
    settings.invoke_refresh();
    window.set_active_view(6);
    wait(&window, &driver, || {
        window.get_active_view() == 6 && !settings.get_busy()
    });
    driver.finish(&window).unwrap();
    drop(driver);
    window.hide().unwrap();
}
