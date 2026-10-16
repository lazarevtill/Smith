#[cfg(feature = "native-ui")]
mod build_overlay;
fn main() {
    println!("cargo:rerun-if-env-changed=SMITH_ORIGINAL_UI_DIR");
    #[cfg(feature = "native-ui")]
    {
        let configured = std::env::var_os("SMITH_ORIGINAL_UI_DIR")
            .expect("Set SMITH_ORIGINAL_UI_DIR to the reviewed original smith/crates/smith-ui directory; this scaffold never downloads or vendors it");
        let root = std::path::PathBuf::from(configured)
            .canonicalize()
            .expect("Original UI directory must already exist");
        let entry = root.join("ui/app-window.slint");
        assert!(entry.is_file(), "Original MainWindow markup is missing");
        println!("cargo:rerun-if-changed={}", root.join("ui").display());
        println!("cargo:rerun-if-changed={}", root.join("assets").display());
        println!("cargo:rustc-env=SMITH_REVIEWED_UI_DIR={}", root.display());
        // Generate explicit unavailable handlers for the *remaining* original
        // public callbacks. This prevents a visible control silently succeeding
        // merely because the original markup compiles. The source checkout is read-only.
        let output =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("reviewed-ui");
        build_overlay::copy_selected_tree(&root.join("ui"), &output.join("ui"));
        build_overlay::copy_selected_tree(&root.join("assets"), &output.join("assets"));
        std::fs::copy(
            "ui/native-platform.slint",
            output.join("ui/native-platform.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-platform.slint");
        let mut source = build_overlay::apply_documents(build_overlay::apply_window(
            std::fs::read_to_string(&entry).unwrap(),
        ));
        source = format!("import {{ NativePlatform }} from \"native-platform.slint\";\nexport {{ NativePlatform }}\n{}", source);
        source = format!("import {{ NativeSettings }} from \"native-settings.slint\";\nexport {{ NativeSettings }}\n{}",source);
        std::fs::copy(
            "ui/native-settings.slint",
            output.join("ui/native-settings.slint"),
        )
        .unwrap();
        std::fs::copy(
            "ui/native-documents.slint",
            output.join("ui/native-documents.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-documents.slint");
        let artifacts = output.join("ui/artifacts.slint");
        std::fs::write(
            &artifacts,
            build_overlay::apply_artifacts(std::fs::read_to_string(&artifacts).unwrap()),
        )
        .unwrap();
        source = format!(
            "import {{ NativeMlx }} from \"native-mlx.slint\";\nexport {{ NativeMlx }}\n{}",
            source
        );
        std::fs::copy("ui/native-mlx.slint", output.join("ui/native-mlx.slint")).unwrap();
        println!("cargo:rerun-if-changed=ui/native-mlx.slint");
        source = format!(
            "import {{ NativeTasks }} from \"native-tasks.slint\";\nexport {{ NativeTasks }}\n{}",
            source
        );
        std::fs::copy(
            "ui/native-tasks.slint",
            output.join("ui/native-tasks.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-tasks.slint");
        source=format!("import {{ NativeAgents, AgentOption }} from \"native-agents.slint\";\nexport {{ NativeAgents, AgentOption }}\n{}",source);
        std::fs::copy(
            "ui/native-agents.slint",
            output.join("ui/native-agents.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-agents.slint");
        source=format!("import {{ NativeExtensions, ExtensionItem }} from \"native-extensions.slint\";\nexport {{ NativeExtensions, ExtensionItem }}\n{}",source);
        std::fs::copy(
            "ui/native-extensions.slint",
            output.join("ui/native-extensions.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-extensions.slint");
        source=format!("import {{ NativeCatalog, CatalogItem }} from \"native-catalog.slint\";\nexport {{ NativeCatalog, CatalogItem }}\n{}",source);
        std::fs::copy(
            "ui/native-catalog.slint",
            output.join("ui/native-catalog.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-catalog.slint");
        let models = output.join("ui/models.slint");
        std::fs::write(
            &models,
            build_overlay::apply_models(std::fs::read_to_string(&models).unwrap()),
        )
        .unwrap();
        let autonomy = output.join("ui/agents-autonomy.slint");
        std::fs::write(
            &autonomy,
            build_overlay::apply_extensions(std::fs::read_to_string(&autonomy).unwrap()),
        )
        .unwrap();
        let agents = output.join("ui/agents.slint");
        std::fs::write(
            &agents,
            build_overlay::apply_agents(std::fs::read_to_string(&agents).unwrap()),
        )
        .unwrap();
        source=format!("import {{ NativeTranscription }} from \"native-transcription.slint\";\nexport {{ NativeTranscription }}\n{}",source);
        std::fs::copy(
            "ui/native-transcription.slint",
            output.join("ui/native-transcription.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-transcription.slint");
        let transcription = output.join("ui/transcribe.slint");
        std::fs::write(
            &transcription,
            build_overlay::apply_transcribe(std::fs::read_to_string(&transcription).unwrap()),
        )
        .unwrap();
        source = format!("import {{ NativeMemory }} from \"native-memory.slint\";\nexport {{ NativeMemory }}\n{}", source);
        std::fs::copy(
            "ui/native-memory.slint",
            output.join("ui/native-memory.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-memory.slint");
        source=format!("import {{ NativeHistorical, HistoricalItem }} from \"native-historical-memory.slint\";\nexport {{ NativeHistorical, HistoricalItem }}\n{}",source);
        std::fs::copy(
            "ui/native-historical-memory.slint",
            output.join("ui/native-historical-memory.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-historical-memory.slint");
        let activity = output.join("ui/activity.slint");
        std::fs::write(
            &activity,
            build_overlay::apply_activity(std::fs::read_to_string(&activity).unwrap()),
        )
        .unwrap();
        source = format!("import {{ NativeProfile }} from \"native-profile.slint\";\nexport {{ NativeProfile }}\n{}", source);
        std::fs::copy(
            "ui/native-profile.slint",
            output.join("ui/native-profile.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-profile.slint");
        std::fs::copy(
            "ui/native-batch.slint",
            output.join("ui/native-batch.slint"),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-batch.slint");
        let settings = output.join("ui/settings.slint");
        std::fs::write(
            &settings,
            build_overlay::apply_settings(std::fs::read_to_string(&settings).unwrap()),
        )
        .unwrap();
        println!("cargo:rerun-if-changed=ui/native-settings.slint");
        let entry = output.join("ui/app-window.slint");
        std::fs::write(&entry, &source).unwrap();
        let theme = output.join("ui/theme.slint");
        std::fs::write(
            &theme,
            build_overlay::apply_theme(std::fs::read_to_string(&theme).unwrap()),
        )
        .unwrap();
        let window = source
            .split("export component MainWindow inherits Window {")
            .nth(1)
            .unwrap()
            .split("export component SmithTray")
            .next()
            .unwrap();
        let connected = [
            "filter-sessions",
            "filter-slash",
            "send",
            "stop",
            "regenerate",
            "switch-branch",
            "select-session",
            "new-session",
            "setting-changed",
            "search-models",
            "refresh-models",
            "select-model",
            "artifacts-refresh",
            "activity-refresh",
            "activity-select-task",
            "activity-cancel-task",
            "activity-answer-clarification",
            "window-minimize",
            "window-maximize",
            "window-close",
            "window-drag",
            "native-original-details",
            "native-original-navigation",
            "native-back",
            "native-forward",
            "native-undo",
            "native-redo",
            "native-toggle-trash",
            "native-discard-draft",
            "native-cancel-edit",
            "edit-message",
            "message-feedback",
            "copy-message",
            "save-code",
            "search-memory",
            "browse-memory",
            "delete-memory",
            "toggle-pin-memory",
            "transcribe-pick-file",
            "transcribe-start",
            "transcribe-save",
            "transcribe-copy",
            "transcribe-save-artifact",
            "transcribe-load-artifact",
            "add-skill",
            "toggle-skill",
            "remove-skill",
            "add-method",
            "toggle-method",
            "remove-method",
            "add-recipe",
            "toggle-recipe",
            "remove-recipe",
            "new-agent",
            "select-agent",
            "save-agent",
            "delete-agent",
            "run-agent-foreground",
            "rename-session",
            "delete-session",
            "export-session",
            "set-default-model",
            "attach-file",
            "clear-attachment",
            "clear-pending-artifact",
            "open-artifact-picker",
            "pick-artifact",
            "artifacts-open",
            "artifacts-register-folder",
        ];
        let mut generated = String::from("fn wire_unavailable(window: &MainWindow) {\n");
        for line in window
            .lines()
            .filter_map(|line| line.strip_prefix("    callback "))
        {
            let (name, rest) = line.split_once('(').unwrap();
            if connected.contains(&name) {
                continue;
            }
            let params = rest.split(')').next().unwrap().trim();
            let args = if params.is_empty() {
                String::new()
            } else {
                params.split(',').map(|_| "_").collect::<Vec<_>>().join(",")
            };
            generated.push_str(&format!("{{let weak = window.as_weak(); window.on_{}(move |{}| {{if let Some(window) = weak.upgrade() {{window.set_status_text(\"{} is not available in this app yet.\".into());}}}});}}\n", name.replace('-', "_"), args, name.replace('-', " ")));
        }
        generated.push_str("}\n");
        std::fs::write(
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("unavailable.rs"),
            generated,
        )
        .unwrap();
        slint_build::compile(entry)
            .expect("Original Slint markup with reviewed local overlays must compile");
    }
}
