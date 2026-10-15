use slint::{ComponentHandle, SharedString};
slint::include_modules!();

fn press(window: &KeyboardReview, text: impl Into<SharedString>) {
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.into() });
}
fn release(window: &KeyboardReview, text: impl Into<SharedString>) {
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: text.into() });
}
fn key(window: &KeyboardReview, text: &str) {
    press(window, text);
    release(window, text);
}

fn main() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            threading: false,
            ..Default::default()
        },
    )))
    .unwrap();
    let window = KeyboardReview::new().unwrap();
    window.show().unwrap();
    assert!(!window.global::<Palette>().get_dark());
    window.invoke_focus_check();
    press(&window, " ");
    assert!(
        !window.get_approved(),
        "approval activates on release, not press"
    );
    release(&window, " ");
    assert!(window.get_approved());
    assert_eq!(window.get_activations(), 1);
    key(&window, "\n");
    assert!(!window.get_approved());
    assert_eq!(window.get_activations(), 2);
    key(&window, "\t");
    key(&window, " ");
    assert!(window.get_picked(), "Tab reaches the next radio control");
    key(&window, "\n");
    assert!(!window.get_picked());
    assert_eq!(window.get_activations(), 4);
    key(&window, "\t");
    key(&window, " ");
    key(&window, "\n");
    assert_eq!(
        window.get_rail_activations(),
        2,
        "Tab reaches rail and both keys activate"
    );

    window.invoke_focus_check();
    press(&window, " ");
    window.invoke_focus_radio();
    release(&window, " ");
    assert_eq!(
        window.get_activations(),
        4,
        "focus loss disarms a pending activation"
    );
    window.set_controls_enabled(false);
    for focus in [
        KeyboardReview::invoke_focus_check,
        KeyboardReview::invoke_focus_radio,
        KeyboardReview::invoke_focus_rail,
    ] {
        focus(&window);
        key(&window, " ");
        key(&window, "\n");
    }
    assert_eq!(window.get_activations(), 4);
    assert_eq!(window.get_rail_activations(), 2);
    window.set_controls_enabled(true);
    window.invoke_focus_check();
    key(&window, " ");
    assert!(window.get_approved());
    assert_eq!(window.get_activations(), 5);
    window.hide().unwrap();
    println!("PASS: actual overlaid Slint theme; Space/Return, Tab traversal, focus-loss disarming, disabled guards, and light palette. Headless owned window only; no OS input.");
}
