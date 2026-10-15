#![cfg(feature = "native-ui")]
use slint::{ComponentHandle, Model};
use smith_native_bridge::{
    fixture::Fixture,
    native::{register_reviewed_fonts, Driver, MainWindow, NativeBatch},
};
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
fn key(w: &MainWindow, value: impl Into<slint::SharedString>) {
    let text = value.into();
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
}
fn click(w: &MainWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    let button = slint::platform::PointerEventButton::Left;
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(slint::platform::WindowEvent::PointerReleased { position, button });
}
fn snapshot(w: &MainWindow, name: &str) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    let p = w.window().take_snapshot().unwrap();
    if let Some(root) = std::env::var_os("SMITH_NATIVE_TEST_CAPTURES") {
        std::fs::create_dir_all(&root).unwrap();
        let mut b = format!("P6\n{} {}\n255\n", p.width(), p.height()).into_bytes();
        for v in p.as_slice() {
            b.extend_from_slice(&[v.r, v.g, v.b]);
        }
        std::fs::write(std::path::Path::new(&root).join(format!("{name}.ppm")), b).unwrap();
    }
    p
}
fn open_menu(w: &MainWindow) {
    // Original rail64 + sidebar300 - padding18 - half button16.
    let y = 34.0
        + if w.get_status_text().is_empty() {
            0.0
        } else {
            36.0
        };
    click(w, 330.0, y);
}
fn choose(w: &MainWindow, index: usize) {
    // Slint's native menu navigation skips separators, preserves disabled rows.
    for _ in 0..=index {
        key(w, slint::platform::Key::DownArrow);
    }
    key(w, slint::platform::Key::Return);
}
#[test]
fn compact_menu_and_composer_recovery_preserve_original_layout_and_keyboard_actions() {
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
    let f = Fixture::start().unwrap();
    let w = MainWindow::new().unwrap();
    let d = Driver::attach(&w, f.clone());
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    w.show().unwrap();
    wait(&w, &d, || d.ready());
    w.invoke_new_session();
    wait(&w, &d, || d.view().conversation.is_some());
    assert!(w.get_native_draft_status().is_empty());
    click(&w, 1182.0, 18.0); // Dismiss the fixture status using its original close control.
    assert!(w.get_status_text().is_empty());
    snapshot(&w, "compact-chat-empty");
    open_menu(&w);
    snapshot(&w, "compact-chat-menu");
    key(&w, slint::platform::Key::Escape);
    // Escape returns to the actual trigger. Tab and Shift+Tab prove it remains
    // in the normal focus chain; Return then opens it without another pointer.
    key(&w, slint::platform::Key::Tab);
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Shift.into(),
        });
    key(&w, slint::platform::Key::Tab);
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased {
            text: slint::platform::Key::Shift.into(),
        });
    key(&w, slint::platform::Key::Return);
    snapshot(&w, "compact-chat-menu-keyboard");
    choose(&w, 4);
    wait(&w, &d, || w.get_native_trash_view());
    snapshot(&w, "compact-trash");
    open_menu(&w);
    choose(&w, 4);
    wait(&w, &d, || !w.get_native_trash_view());
    open_menu(&w);
    choose(&w, 5);
    wait(&w, &d, || w.global::<NativeBatch>().get_mode());
    open_menu(&w);
    choose(&w, 5);
    wait(&w, &d, || !w.global::<NativeBatch>().get_mode());
    let index = w.get_selected_session();
    w.invoke_rename_session(index, "Menu history target".into());
    wait(&w, &d, || {
        d.view().conversation.as_ref().unwrap().chat.title == "Menu history target"
    });
    assert!(w.get_native_can_undo());
    open_menu(&w);
    choose(&w, 2);
    wait(&w, &d, || {
        d.view().conversation.as_ref().unwrap().chat.title != "Menu history target"
    });
    open_menu(&w);
    choose(&w, 3);
    wait(&w, &d, || {
        d.view().conversation.as_ref().unwrap().chat.title == "Menu history target"
    });
    let current_chat = d.view().conversation.as_ref().unwrap().chat.id.clone();
    open_menu(&w);
    choose(&w, 0);
    wait(&w, &d, || d.view().conversation.is_none());
    open_menu(&w);
    choose(&w, 1);
    wait(&w, &d, || {
        d.view()
            .conversation
            .as_ref()
            .is_some_and(|c| c.chat.id == current_chat)
    });
    // Slint maps logical Control to Command on macOS (Meta is physical Ctrl).
    // Real text-input Cmd+Z undoes typed text, not chat rename.
    if !w.get_status_text().is_empty() {
        click(&w, 1182.0, 18.0);
    }
    snapshot(&w, "compact-chat-saved");
    click(&w, 600.0, 718.0);
    key(&w, "draft keyboard text");
    let text_history = d.view().actions.revision;
    let typed = w.get_composer_text();
    assert!(typed.contains("draft keyboard text"));
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::Control.into(),
        });
    key(&w, "z");
    w.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased {
            text: slint::platform::Key::Control.into(),
        });
    assert_ne!(w.get_composer_text(), typed);
    assert_eq!(
        d.view().conversation.as_ref().unwrap().chat.title,
        "Menu history target"
    );
    // Let a wrongly bubbled asynchronous history command finish before judging
    // the result; immediate text-state alone would miss that regression.
    for _ in 0..40 {
        d.pump(&w);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(d.view().actions.revision, text_history);
    assert_eq!(
        d.view().conversation.as_ref().unwrap().chat.title,
        "Menu history target"
    );
    wait(&w, &d, || !d.view().draft.dirty);
    let owner = d.view().draft.owner;
    f.replace_draft(&owner, "Saved by another client").unwrap();
    w.set_composer_text("Retained unsaved conflict text".into());
    wait(&w, &d, || d.view().draft.conflict);
    assert_eq!(w.get_composer_text(), "Retained unsaved conflict text");
    assert!(!w.get_native_can_undo());
    snapshot(&w, "compact-draft-conflict");
    // The context menu must not activate disabled conversation Undo.
    let history = d.view().actions.revision;
    open_menu(&w);
    choose(&w, 2);
    key(&w, slint::platform::Key::Escape);
    d.pump(&w);
    assert_eq!(d.view().actions.revision, history);
    assert_eq!(w.get_composer_text(), "Retained unsaved conflict text");
    // Activate the actual recovery button beside the composer, not a test-only
    // callback. Only this explicit click may discard the retained unsaved text.
    click(&w, 1104.0, 678.0);
    wait(&w, &d, || !d.view().draft.conflict);
    assert_eq!(w.get_composer_text(), "Saved by another client");
    assert!(w.get_native_draft_status().is_empty());
    snapshot(&w, "compact-draft-recovered");
    w.set_active_view(2);
    if !w.get_status_text().is_empty() {
        click(&w, 1182.0, 18.0);
    }
    snapshot(&w, "compact-settings");
    assert!(w.get_sessions().row_count() > 0);
    drop(d);
    w.hide().unwrap();
}
