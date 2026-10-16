//! Checks the actual reviewed source overlay, without a renderer or OS effects.
#[allow(dead_code)]
#[path = "../build_overlay.rs"]
mod overlay;
fn original(name: &str) -> String {
    std::fs::read_to_string(
        std::path::PathBuf::from(
            std::env::var_os("SMITH_ORIGINAL_UI_DIR")
                .expect("Explicit reviewed UI directory is required"),
        )
        .join("ui")
        .join(name),
    )
    .unwrap()
}
#[test]
fn unsupported_windows_provisioning_and_startup_controls_are_absent() {
    let settings = overlay::apply_settings(original("settings.slint"));
    let window = overlay::apply_window(original("app-window.slint"));
    let settings = settings
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let window = window
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for text in [
        "title: \"WSL2",
        "manual-command: \"wsl",
        "winget install",
        "Start Smith at Windows login",
        "Launch automatically when you sign in to Windows.",
        "Check & provision",
        "Tray only (no taskbar entry)",
    ] {
        assert!(
            !settings.contains(text) && !window.contains(text),
            "Misleading platform control: {text}"
        );
    }
    for title in [
        "Appearance",
        "Artifacts storage location",
        "Node identity",
        "Access key",
    ] {
        assert!(
            settings.contains(&format!("title: \"{title}\";")),
            "Neighboring original card was removed: {title}"
        );
    }
    assert!(window.contains("Chat actions"));
    assert!(window.contains("Speech-to-text model (voice / transcription)"));
}
#[test]
fn original_inference_and_provider_lists_are_replaced_by_target_policy() {
    let settings = overlay::apply_settings(original("settings.slint"));
    assert!(settings.contains("model:NativePlatform.engines"));
    assert!(settings.contains("if NativePlatform.mlx && NativeSettings.engine-index==1"));
    assert!(!settings.contains("title: \"Windows ML (experimental)\""));
    assert!(!settings.contains("title: \"Execution provider\""));
}
#[test]
fn extension_recipe_text_does_not_claim_a_working_sandbox() {
    let source = overlay::apply_extensions(original("agents-autonomy.slint"));
    assert!(!source.contains("WSL2"));
    assert!(!source.contains("Windows .cmd"));
    assert!(source.contains("Recipe execution is unavailable in this build"));
}
