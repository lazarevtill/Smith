//! Deterministic build-only changes to the selected original UI. No write ever
//! targets the original checkout. Anchors fail closed when upstream changes.
use std::{fs, path::Path};
fn replace(source: &mut String, from: &str, to: &str, expected: usize) {
    assert_eq!(
        source.matches(from).count(),
        expected,
        "Original UI overlay anchor changed: {from}"
    );
    *source = source.replace(from, to);
}
pub fn apply_window(mut source: String) -> String {
    replace(
        &mut source,
        "enabled: !root.export-session-saving;",
        "enabled: NativePlatform.pickers && !root.export-session-saving;",
        1,
    );
    replace(
        &mut source,
        "enabled: !root.code-block-saving;",
        "enabled: NativePlatform.pickers && !root.code-block-saving;",
        1,
    );
    for (name, capability) in [
        ("cc-ta", "clipboard"),
        ("copy-ta", "clipboard"),
        ("attach-touch", "pickers"),
    ] {
        replace(
            &mut source,
            &format!("{name} := TouchArea {{"),
            &format!("{name} := TouchArea {{ enabled: NativePlatform.{capability};"),
            1,
        );
    }

    // These original Windows provisioning actions have no implementation in
    // the new app, including on Windows. Never offer host-shell substitutes.
    for title in [
        "ffmpeg (audio/video file + URL transcription)",
        "WSL2 sandbox (safe tool / shell execution)",
    ] {
        let start_anchor = format!(
            "                        SetupRow {{\n                            title: \"{title}\";"
        );
        assert_eq!(source.matches(&start_anchor).count(), 1);
        let start = source.find(&start_anchor).unwrap();
        let end = start
            + source[start..].find("                        }\n").unwrap()
            + "                        }\n".len();
        source.replace_range(start..end, "");
    }

    // Invalidate synchronously in each actual navigation action. Property-change
    // notifications can coalesce a leave/return before the next event-loop turn.
    replace(
        &mut source,
        "root.active-view = ",
        "root.native-original-navigation(); root.active-view = ",
        10,
    );
    replace(&mut source, "    changed active-view => {", "    callback native-original-navigation();\n    changed active-view => {\n        root.native-original-navigation();", 1);
    replace(
        &mut source,
        "    image: image,",
        "    image: image,\n    action-index: int,",
        1,
    );
    replace(
        &mut source,
        "export struct ChatMessage {",
        "export struct ChatMessage {\n    historical-available: bool,",
        1,
    );
    replace(&mut source, "    callback send(string);", "    callback native-original-details(string, int);\n    in property <bool> native-original-pending: false;\n    callback send(string);", 1);
    replace(
        &mut source,
        "if seg.kind == \"image\": Rectangle {",
        "if seg.kind == \"image\" || seg.kind == \"historical-image\": Rectangle {",
        1,
    );
    replace(
        &mut source,
        "img-open-ta := TouchArea {",
        "if seg.kind == \"image\": img-open-ta := TouchArea {",
        1,
    );
    replace(
        &mut source,
        "                                                if seg.kind == \"tool\": Rectangle {",
        r#"
                                                if seg.kind == "historical-note": Text {
                                                    text: seg.text; color: m.role == "user" ? Palette.user-text : Palette.text-muted;
                                                    font-family: Palette.font-mono; font-size: 11px; wrap: word-wrap;
                                                }
                                                if seg.kind == "historical-image-load": SmithButton {
                                                    text: seg.text; kind: "secondary"; height: 32px; width: 220px;
                                                    background: Palette.raised; border-color: Palette.border;
                                                    enabled: !root.native-original-pending;
                                                    clicked => { root.native-original-details(m.id, seg.action-index); }
                                                }
                                                if seg.kind == "tool": Rectangle {"#,
        1,
    );
    replace(
        &mut source,
        "                                        // Mirrors the DS CodeBlock copy affordance above",
        r#"
                                        if m.historical-available: SmithButton {
                                            text: "Details"; kind: "secondary";
                                            height: 28px; width: 84px;
                                            enabled: !root.native-original-pending;
                                            clicked => { root.native-original-details(m.id, -1); }
                                        }
                                        // Mirrors the DS CodeBlock copy affordance above"#,
        1,
    );

    replace(&mut source, "if root.status-text != \"\": Rectangle {\n        height: 36px;\n        width: 100%;\n        background: Palette.accent;", "if root.status-text != \"\": Rectangle {\n        height: 36px;\n        width: 100%;\n        background: Palette.button-primary;", 1);
    replace(
        &mut source,
        "    callback send(string);",
        r#"    callback native-back();
    callback native-forward();
    callback native-undo();
    callback native-redo();
    callback native-toggle-trash();
    callback native-discard-draft();
    callback native-cancel-edit();
    in property <bool> native-can-back: false;
    in property <bool> native-can-forward: false;
    in property <bool> native-can-undo: false;
    in property <bool> native-can-redo: false;
    in property <bool> native-trash-view: false;
    in property <bool> native-draft-conflict: false;
    in property <string> native-draft-status: "";
    in property <string> native-undo-label: "Undo";
    in property <string> native-redo-label: "Redo";
    callback send(string);"#,
        1,
    );
    // Send owns clearing only after backend acceptance. Later typing survives.
    for spaces in [52usize, 48, 52] {
        let from = format!(
            "root.send(root.composer-text);\n{}root.composer-text = \"\";",
            " ".repeat(spaces)
        );
        if source.contains(&from) {
            source = source.replacen(&from, "root.send(root.composer-text);", 1);
        }
    }
    assert!(
        !source
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w[0].contains("root.send(root.composer-text)")
                && w[1].contains("root.composer-text =")),
        "Eager send clear remained"
    );
    replace(
        &mut source,
        "root.confirming-delete-session = idx;",
        "root.delete-session(idx);",
        1,
    );
    // Keep actions inside the existing Chats header instead of adding a global
    // toolbar. Slint's menu provides keyboard navigation, Escape and disabled
    // entries; the existing button owns focus only when explicitly activated.
    replace(
        &mut source,
        r#"                Text {
                    text: "Chats";
                    color: Palette.text-strong;
                    font-family: Palette.font-display;
                    font-size: 22px;
                    font-weight: 600;
                }"#,
        r#"                HorizontalLayout {
                    height: 32px; spacing: 8px;
                    Text {
                        text: root.native-trash-view ? "Trash" : "Chats";
                        color: Palette.text-strong; font-family: Palette.font-display;
                        font-size: 22px; font-weight: 600;
                        vertical-alignment: center; horizontal-stretch: 1;
                    }
                    chat-actions := ContextMenuArea {
                        width: 32px; height: 32px;
                        SmithButton {
                            text: ""; width: 32px; height: 32px; kind: "ghost"; accessible-label: "Chat actions";
                            Image { source: Icons.more-horizontal; width: 18px; height: 18px; x: (parent.width - self.width) / 2; y: (parent.height - self.height) / 2; colorize: Palette.text-subtle; }
                            clicked => { chat-actions.show({ x: 0px, y: self.height }); }
                        }
                        Menu {
                            MenuItem { title: "Back"; enabled: root.native-can-back && !root.session-loading; activated => { root.native-back(); } }
                            MenuItem { title: "Forward"; enabled: root.native-can-forward && !root.session-loading; activated => { root.native-forward(); } }
                            MenuSeparator {}
                            MenuItem { title: root.native-undo-label; enabled: root.native-can-undo && !root.streaming && !root.session-loading; activated => { root.native-undo(); } }
                            MenuItem { title: root.native-redo-label; enabled: root.native-can-redo && !root.streaming && !root.session-loading; activated => { root.native-redo(); } }
                            MenuSeparator {}
                            MenuItem { title: root.native-trash-view ? "Back to chats" : "Trash"; enabled: !root.session-loading; activated => { root.native-toggle-trash(); } }
                            MenuItem { title: NativeBatch.mode ? "Done selecting" : "Select chats"; enabled: NativeBatch.enabled && !NativeBatch.busy && !root.session-loading; activated => { NativeBatch.toggle-mode(); } }
                        }
                    }
                }"#,
        1,
    );
    replace(&mut source, "root.composer-text = \"\";\n                                            root.editing-message = false;\n                                            root.filter-slash(\"\");", "root.native-cancel-edit();", 1);
    // Transient saving and recovery belong beside the draft. Idle saved drafts
    // have no permanent status line and Settings receives no chat controls.
    replace(
        &mut source,
        "                        padding-bottom: 20px;\n\n                        Rectangle {",
        r#"                        padding-bottom: 20px;
                        spacing: 6px;
                        if root.native-draft-status != "": HorizontalLayout {
                            spacing: 8px;
                            Text { text: root.native-draft-status; color: root.native-draft-conflict ? Palette.error-text : Palette.text-subtle; font-family: Palette.font-body; font-size: 12px; wrap: word-wrap; vertical-alignment: center; horizontal-stretch: 1; }
                            if root.native-draft-conflict: SmithButton {
                                text: "Discard & reload"; accessible-label: "Discard unsaved draft and reload saved draft";
                                kind: "ghost"; height: 32px; width: 144px; enabled: !root.session-loading;
                                clicked => { root.native-discard-draft(); }
                            }
                        }

                        Rectangle {"#,
        1,
    );
    replace(
        &mut source,
        "        key-pressed(event) => {\n            if (event.modifiers.control",
        r#"        key-pressed(event) => {
            // Inputs retain their own text Undo/Redo before bubbling here.
            if ((event.modifiers.control || event.modifiers.meta) && event.text == "z") {
                if event.modifiers.shift { root.native-redo(); } else { root.native-undo(); }
                return accept;
            }
            if ((event.modifiers.control || event.modifiers.meta) && event.text == "y") { root.native-redo(); return accept; }
            if event.modifiers.alt && event.text == Key.LeftArrow { root.native-back(); return accept; }
            if event.modifiers.alt && event.text == Key.RightArrow { root.native-forward(); return accept; }
            if (event.modifiers.control"#,
        1,
    );
    replace(&mut source, "composer-input := TextInput {", "composer-input := TextInput {\n                                        read-only: root.session-loading;", 1);
    // A complete leading thinking envelope can be projected for display only.
    // The original m.text still drives Copy/save/export; raw view stays available.
    replace(
        &mut source,
        "import { LineEdit, ListView, ScrollView, Palette as StdPalette }",
        "import { LineEdit, ListView, ScrollView, TextEdit, Palette as StdPalette }",
        1,
    );
    replace(
        &mut source,
        "    reasoning: string,",
        "    reasoning: string,\n    reasoning-derived: bool,",
        1,
    );
    replace(&mut source, "                                    Rectangle {\n                                        max-width: m.role == \"user\" ? root.width * 0.78 : root.width;", "                                    message-bubble := Rectangle {\n                                        property <bool> raw-response: false;\n                                        max-width: m.role == \"user\" ? root.width * 0.78 : root.width;", 1);
    replace(&mut source, "                                            for seg in m.segments: VerticalLayout {", "                                            if m.reasoning-derived: SmithCheck {text:\"Show raw response\";checked<=>message-bubble.raw-response;}\n                                            if m.reasoning-derived && message-bubble.raw-response: TextEdit {text:m.text;read-only:true;height:180px;wrap:word-wrap;}\n                                            for seg in (m.reasoning-derived && message-bubble.raw-response ? [] : m.segments): VerticalLayout {", 1);
    let thinking_start = source
        .find("                                    // DS ThinkingBlock")
        .unwrap();
    let thinking_end = source[thinking_start..]
        .find("                                    message-bubble := Rectangle")
        .unwrap()
        + thinking_start;
    source.replace_range(thinking_start..thinking_end,r#"                                    // Reasoning is inert model output, disclosed separately.
                                    if m.role != "user" && m.reasoning != "": tb := Rectangle {
                                        property <bool> expanded:false;
                                        max-width:root.width * 0.6;height:tb-body.preferred-height;
                                        border-radius:Palette.radius;border-width:1px;border-color:Palette.border;background:Palette.sunken;
                                        tb-body := VerticalLayout {padding:9px;spacing:6px;
                                            SmithButton {text:tb.expanded?"Thinking — hide reasoning":"Thinking — show reasoning";kind:"ghost";clicked=>{tb.expanded=!tb.expanded;}}
                                            if tb.expanded:Text {text:m.reasoning;color:Palette.text-subtle;font-family:Palette.font-mono;font-size:11px;wrap:word-wrap;}
                                        }
                                    }

"#);
    // Feedback reuses keyboard-capable native buttons; actions remain local ratings.
    let feedback = source
        .find("                                        // Feedback (assistant turns)")
        .expect("Feedback anchor changed");
    let edit = source[feedback..]
        .find("                                        // Edit (user turns)")
        .expect("Edit anchor changed")
        + feedback;
    source.replace_range(feedback..edit,r#"                                        if m.role != "user" && m.text != "": SmithButton {
                                            text:"";width:32px;height:32px;kind:"ghost";accessible-label:"Good response";enabled:!root.streaming;
                                            Image{width:12px;height:12px;x:(parent.width - self.width)/2;y:(parent.height - self.height)/2;source:@image-url("../assets/icons/thumb-up.svg");colorize:m.feedback=="thumbs_up"?Palette.accent:Palette.text-subtle;}
                                            clicked=>{root.message-feedback(m.id,"thumbs_up");}
                                        }
                                        if m.role != "user" && m.text != "": SmithButton {
                                            text:"";width:32px;height:32px;kind:"ghost";accessible-label:"Bad response";enabled:!root.streaming;
                                            Image{width:12px;height:12px;x:(parent.width - self.width)/2;y:(parent.height - self.height)/2;source:@image-url("../assets/icons/thumb-down.svg");colorize:m.feedback=="thumbs_down"?Palette.accent:Palette.text-subtle;}
                                            clicked=>{root.message-feedback(m.id,"thumbs_down");}
                                        }
"#);
    // Existing original controls remain in place. The action changes to Restore
    // only while the sidebar is explicitly showing recoverable Trash.
    replace(&mut source, "source: Icons.trash;\n                                            colorize: trash-ta", "source: root.native-trash-view ? Icons.refresh : Icons.trash;\n                                            colorize: trash-ta", 1);
    source=format!("import {{ NativeBatch, NativeBatchControls }} from \"native-batch.slint\";\nexport {{ NativeBatch }}\n{}",source);
    replace(
        &mut source,
        "export struct SessionItem {",
        "export struct SessionItem { selected: bool,",
        1,
    );
    replace(
        &mut source,
        "root.select-session(idx);",
        "if NativeBatch.mode { NativeBatch.toggle(s.id); } else { root.select-session(idx); }",
        1,
    );
    replace(
        &mut source,
        "    // ===== transient status banner",
        "    // ===== transient status banner",
        1,
    );
    replace(
        &mut source,
        "                // Session search (P4",
        "                if NativeBatch.mode: NativeBatchControls {}\n                // Session search (P4",
        1,
    );
    replace(
        &mut source,
        "Avatar, Banner, EmptyState, CopyBlock, SmithRadio",
        "Avatar, Banner, EmptyState, CopyBlock, SmithRadio, SmithCheck",
        1,
    );
    replace(&mut source,"                                Avatar {\n                                    initials: \"S\";", "                                if NativeBatch.mode: SmithCheck { width:32px; text: \"\"; checked:s.selected; enabled:!NativeBatch.busy; toggled=>{NativeBatch.toggle(s.id);} }\n                                if !NativeBatch.mode: Avatar {\n                                    initials: \"S\";",1);
    source
}
pub fn apply_theme(mut source: String) -> String {
    replace(
        &mut source,
        "in-out property <bool> dark: true;",
        "in-out property <bool> dark: false;",
        1,
    );
    replace(
        &mut source,
        "out property <brush> text-subtle: dark ? ink-500 : ink-400;",
        "out property <brush> text-subtle: ink-500;",
        1,
    );
    replace(
        &mut source,
        "    // ---- Status ----",
        r#"    // Accessible text-button aliases. Brand marks/focus keep #f81a2c.
    out property <brush> button-primary: #c70f1e;
    out property <brush> button-primary-hover: #b10d1b;
    out property <brush> button-primary-pressed: #950b16;
    out property <brush> interactive-border: #5b5b63;
    out property <brush> success-text: #156b41;
    out property <brush> warning-text: #8a5a12;
    out property <brush> error-text: #c70f1e;
    out property <brush> info-text: #2348a3;
    out property <brush> focus-ring: #f81a2c;
    // ---- Status ----"#,
        1,
    );
    replace(&mut source,"is-primary ? (ta.has-hover ? Palette.accent-hover : Palette.accent)","is-primary ? ((ta.pressed || root.key-down) ? Palette.button-primary-pressed : (ta.has-hover ? Palette.button-primary-hover : Palette.button-primary))",1);
    replace(&mut source,"is-destructive ? (ta.has-hover ? Palette.accent : Palette.black)","is-destructive ? ((ta.pressed || root.key-down) ? Palette.button-primary-pressed : (ta.has-hover ? Palette.button-primary : Palette.black))",1);
    // Preserve the original components and dimensions; add keyboard and
    // accessibility activation to inherited pointer-only approval controls.
    for (component, role, label, state, action) in [
        (
            "SmithCheck",
            "checkbox",
            "text",
            Some("checked"),
            "root.checked = !root.checked; root.toggled();",
        ),
        (
            "SmithRadio",
            "radio-button",
            "text",
            Some("selected"),
            "root.picked();",
        ),
        ("RailButton", "button", "label", None, "root.clicked();"),
    ] {
        let start = source
            .find(&format!(
                "export component {component} inherits Rectangle {{"
            ))
            .unwrap();
        let end = source[start + 1..]
            .find("export component ")
            .map_or(source.len(), |n| start + 1 + n);
        let mut part = source[start..end].to_owned();
        if component == "RailButton" {
            replace(&mut part, "    accessible-role: button;\n    accessible-label: root.label;\n    accessible-enabled: root.enabled;\n    accessible-action-default => { root.clicked(); }\n", "", 1);
        } else {
            replace(&mut part, "height: 28px;", "height: 32px;", 1);
            part = part.replace(" : Palette.border;", " : Palette.interactive-border;");
        }
        let checked = state
            .map(|p| format!("    accessible-checked: root.{p};\n"))
            .unwrap_or_default();
        let keyboard = format!(
            r#"
    accessible-role: {role};
    accessible-label: root.{label};
    accessible-enabled: root.enabled;
{checked}    accessible-action-default => {{ if root.enabled {{ {action} }} }}
    forward-focus: control-focus;
    property <bool> key-down: false;
    control-focus := FocusScope {{
        enabled: root.enabled;
        changed has-focus => {{ if !self.has-focus {{ root.key-down = false; }} }}
        key-pressed(event) => {{
            if root.enabled && (event.text == " " || event.text == Key.Return) {{ root.key-down = true; return accept; }}
            return reject;
        }}
        key-released(event) => {{
            if event.text == " " || event.text == Key.Return {{
                if root.enabled && root.key-down {{ {action} }}
                root.key-down = false; return accept;
            }}
            return reject;
        }}
    }}
    if control-focus.has-focus: Rectangle {{
        x: -4px; y: -4px; width: parent.width + 8px; height: parent.height + 8px;
        border-radius: Palette.radius + 2px; border-width: 2px;
        border-color: Palette.focus-ring; background: transparent;
    }}
"#
        );
        let body = part.find('{').unwrap() + 1;
        part.insert_str(body, &keyboard);
        // Pointer selection also establishes the keyboard focus location.
        let touch = part.find("    ta := TouchArea {").unwrap();
        let click = part[touch..].find("clicked => {").unwrap() + touch + "clicked => {".len();
        part.insert_str(click, " control-focus.focus(); ");
        source.replace_range(start..end, &part);
    }
    replace(&mut source,"x: -3px;\n        y: -3px;\n        width: parent.width + 6px;\n        height: parent.height + 6px;\n        border-radius: Palette.radius + 2px;\n        border-width: 1px;\n        border-color: Palette.accent;","x: -4px;\n        y: -4px;\n        width: parent.width + 8px;\n        height: parent.height + 8px;\n        border-radius: Palette.radius + 2px;\n        border-width: 2px;\n        border-color: Palette.focus-ring;",1);
    source = source.replace(
        "color: root.copied ? Palette.success :",
        "color: root.copied ? Palette.success-text :",
    );
    source
}
pub fn copy_selected_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        assert!(
            !kind.is_symlink(),
            "Selected UI tree must not contain symlinks"
        );
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_selected_tree(&entry.path(), &target);
        } else if kind.is_file() {
            fs::copy(entry.path(), target).unwrap();
        } else {
            panic!("Unsupported original UI file type");
        }
    }
}

pub fn apply_settings(mut source: String) -> String {
    // Capability-based omission: startup registration/tray-only and WSL2
    // provisioning are unimplemented on every target in this bridge.
    for (title, next) in [
        ("Startup & window", "Artifacts storage location"),
        ("WSL2 sandbox", "Node identity"),
    ] {
        let anchor =
            format!("                    Card {{\n                        title: \"{title}\";");
        let next_anchor =
            format!("                    Card {{\n                        title: \"{next}\";");
        assert_eq!(source.matches(&anchor).count(), 1);
        assert_eq!(source.matches(&next_anchor).count(), 1);
        let start = source.find(&anchor).unwrap();
        let end = source[start..].find(&next_anchor).unwrap() + start;
        source.replace_range(start..end, "");
    }
    source = format!("import {{ NativePlatform }} from \"native-platform.slint\";\n{source}");

    source = format!("import {{ NativeProfileControls }} from \"native-profile.slint\";\nimport {{ NativeMlxControls }} from \"native-mlx.slint\";\nimport {{ NativeSettings, NativeConnectionControls, NativeSetupControls }} from \"native-settings.slint\";\n{}",source);
    replace(
        &mut source,
        "\"Metrics\", \"About\"]",
        "\"Metrics\", \"About\", \"Data\"]",
        1,
    );
    replace(&mut source, "                            TabRow { label: \"Privacy\";", "                            TabRow { label: \"Data\"; index: 8; active-index: root.active-tab; picked(i) => { root.active-tab = i; root.select-tab(i); } }\n                            TabRow { label: \"Privacy\";", 1);
    replace(&mut source, "                // ===== General =====", "                if root.active-tab == 8: Card { title: \"Import original profile\"; NativeProfileControls {} }\n                // ===== General =====", 1);
    let start = source
        .find("                // ===== Providers (cloud API keys) =====")
        .expect("Providers anchor changed");
    let end = source[start..]
        .find("                // ===== Metrics =====")
        .expect("Metrics anchor changed")
        + start;
    source.replace_range(
        start..end,
        r#"                // ===== Providers: reviewed local backend contract =====
                if root.active-tab == 4: VerticalLayout {
                    spacing: 14px;
                    Card { title: "Connection"; NativeConnectionControls {} }
                }

"#,
    );
    let start = source
        .find("                if root.active-tab == 3: VerticalLayout {")
        .expect("Inference anchor changed");
    let end = source[start..]
        .find("                if root.active-tab == 5: VerticalLayout {")
        .expect("Privacy anchor changed")
        + start;
    source.replace_range(
        start..end,
        r#"                if root.active-tab == 3: VerticalLayout {
                    spacing: 14px;
                    ComboBox {model:NativePlatform.engines;current-index<=>NativeSettings.engine-index;enabled:NativePlatform.owned-runtime;}
                    if !NativePlatform.owned-runtime: Banner { kind:"warning"; text:"Local runtime launch is not available on this platform in this build."; }
                    if NativePlatform.owned-runtime && NativeSettings.engine-index==0: Card { title: "Local inference"; NativeSetupControls {} }
                    if NativePlatform.mlx && NativeSettings.engine-index==1: Card { title: "Apple MLX"; NativeMlxControls {} }
                }

"#,
    );
    source = source.replace(
        "Match Windows, or force a light or dark theme.",
        "Smith uses the shared light appearance.",
    );
    source
}

pub fn apply_documents(mut source: String) -> String {
    source=format!("import {{ NativeDocuments, NativeDocumentReview }} from \"native-documents.slint\";\nexport {{ NativeDocuments }}\n{}",source);
    replace(&mut source,"    // ===== transient status banner", "    if NativeDocuments.pending && root.active-view != 6: Rectangle { background: Palette.raised; VerticalLayout { padding: 8px; NativeDocumentReview {} } }\n    // ===== transient status banner",1);
    source
}
pub fn apply_artifacts(mut source: String) -> String {
    source = format!(
        "import {{ NativeArtifactControls }} from \"native-documents.slint\";\n{}",
        source
    );
    replace(
        &mut source,
        "text: \"Register folder…\";",
        "text: \"Import document…\";",
        1,
    );
    replace(&mut source,"Chat attachments, transcripts, and catalogued folders — kept encrypted at rest, browsable here.","Saved answers and generated documents — kept in your local workspace, browsable here.",1);
    let anchor = "        if root.status != \"\": Text {";
    assert_eq!(
        source.matches(anchor).count(),
        2,
        "Artifact status anchors changed"
    );
    source = source.replacen(
        anchor,
        "        NativeArtifactControls {}\n        if root.status != \"\": Text {",
        1,
    );
    source
}

pub fn apply_agents(mut source: String) -> String {
    source=format!("import {{ NativeTaskControls }} from \"native-tasks.slint\";\nimport {{ NativeAgents, NativeAgentScope }} from \"native-agents.slint\";\n{}",source);
    replace(&mut source,"    HorizontalLayout {\n        width: 100%;", "    VerticalLayout { spacing:0;\n        HorizontalLayout { padding:8px;spacing:8px;SmithButton{text: \"Saved agents\";clicked=>{NativeAgents.tasks-mode=false;NativeAgents.refresh();}}SmithButton{text: \"Task workspaces\";clicked=>{NativeAgents.tasks-mode=true;}}SmithCheck{text: \"Show deleted agents\";checked<=>NativeAgents.show-deleted;toggled=>{NativeAgents.refresh();}}}\n        if NativeAgents.tasks-mode: NativeTaskControls { vertical-stretch:1; }\n        if !NativeAgents.tasks-mode: HorizontalLayout {\n        width: 100%;",1);
    source.push_str("\n}\n");
    let model = source
        .find("                        text: \"Model\";")
        .unwrap();
    let start = source[..model]
        .rfind("                VerticalLayout {")
        .unwrap();
    let prompt = source
        .find("                        text: \"System prompt\";")
        .unwrap();
    let end = source[..prompt]
        .rfind("                VerticalLayout {")
        .unwrap();
    source.replace_range(start..end, "                NativeAgentScope {}\n");
    let caps = source
        .find("                        text: \"Capabilities\";")
        .unwrap();
    let start = source[..caps]
        .rfind("                VerticalLayout {")
        .unwrap();
    let end=source[caps..].find("                HorizontalLayout {\n                    spacing: 8px;\n                    SmithButton {").unwrap()+caps;
    source.replace_range(start..end, "");
    let learned = source
        .find("                // ---- \"What Smith learned\"")
        .unwrap();
    source.truncate(learned);
    source.push_str("            }\n        }\n    }\n    }\n}\n");
    source = source.replace(
        "Soul (stable preferences the agent learns about you)",
        "Persona · stable instructions",
    );
    source=source.replace("Create a focused assistant with its own prompt, model, memory, and tools — use New agent above.","Create a saved persona with its own prompt and explicit workspace permissions.");
    source = source.replace(
        "root.agent-save-is-error ? Palette.red : Palette.success",
        "root.agent-save-is-error ? Palette.error-text : Palette.success-text",
    );
    source = source.replace(
        "text: root.agent-run-state == \"running\" ? \"Running…\" : \"▶  Run\";",
        "text: root.agent-run-state == \"running\" ? \"Creating…\" : \"Create task\";",
    );
    source = source.replace(
        "text: root.agent-run-state == \"running\" ? \"Manage run\" : \"Background run\";",
        "text: \"Open Activity\";",
    );
    source = source.replace("root.run-agent-background();", "root.open-runs();");
    source = source.replace(
        "text: \"Run this agent\";",
        "text: \"Create a task from this saved agent\";",
    );
    let delete_start = source
        .find("                    if root.editing-agent != \"\": ConfirmButton {")
        .expect("Original saved-agent delete control");
    let delete_end = source[delete_start..]
        .find("                    if root.agent-save-status != \"\": Text {")
        .unwrap()
        + delete_start;
    source.replace_range(delete_start..delete_end,"                    if root.editing-agent != \"\" && !NativeAgents.deleted: SmithButton {\n                        text: \"Delete\"; kind: \"destructive\";\n                        enabled: !root.agent-saving && !root.agent-loading && !root.agent-deleting;\n                        clicked => { root.delete-agent(root.editing-agent); }\n                    }\n");
    source=source.replace("enabled: !root.agent-saving && !root.agent-loading && !root.agent-deleting;", "enabled: !NativeAgents.deleted && !root.agent-saving && !root.agent-loading && !root.agent-deleting;");
    source
}
pub fn apply_activity(mut source: String) -> String {
    source=format!("import {{ NativeHistorical, NativeMemoryTabs, NativeHistoricalControls }} from \"native-historical-memory.slint\";\n{}",source);
    source = format!(
        "import {{ NativeTaskActions }} from \"native-tasks.slint\";\nimport {{ NativeMemory, NativeMemoryControls }} from \"native-memory.slint\";\n{}",
        source
    );
    replace(
        &mut source,
        "                if root.compact: SmithButton {",
        "                NativeTaskActions {}\n                if root.compact: SmithButton {",
        1,
    );
    let start = source
        .find("        if root.sub-view == 3: ScrollView {")
        .expect("Original Memory pane");
    let end = source[start..]
        .find("        if root.sub-view == 0: HorizontalLayout {")
        .unwrap()
        + start;
    let mut memory = source[start..end].to_owned();
    memory = memory.replace("Recall distilled episodes and pinned facts across sessions, or browse everything that's stored.", "Browse and edit explicitly saved facts in this local workspace. Search matches literal text.");
    memory = memory.replace(
        "Type a query and search, or Browse all, to see recalled episodes and pinned facts.",
        "Choose a namespace or leave it empty, then Search or Browse all.",
    );
    replace(&mut memory, "                            text <=> root.memory-query;", "                            text <=> root.memory-query;\n                            edited => { NativeMemory.scope-changed(); }", 1);
    replace(&mut memory, "                Card {\n                    title: \"Results\";", "                NativeMemoryControls {}\n                Card {\n                    title: NativeMemory.deleted ? \"Trash\" : \"Results\";", 1);
    replace(&mut memory, "                                    text: mem.pinned ? \"Unpin\" : \"Pin\";", "                                    text: mem.pinned ? \"Unpin\" : \"Pin\";\n                                    enabled: !NativeMemory.busy && !NativeMemory.deleted;", 1);
    replace(&mut memory, "                                ConfirmButton {\n                                    label: \"Delete\";\n                                    confirm-text: \"Delete?\";\n                                    reset-key: mem.id;\n                                    confirmed => { root.delete-memory(mem.id); }\n                                }", "                                SmithButton { text: \"Edit\"; enabled: !NativeMemory.busy && !NativeMemory.deleted; clicked => { NativeMemory.edit(mem.id); } }\n                                SmithButton { text: NativeMemory.deleted ? \"Restore\" : \"Trash\"; enabled: !NativeMemory.busy; clicked => { root.delete-memory(mem.id); } }", 1);
    memory = memory.replacen(
        "if root.sub-view == 3: ScrollView {",
        "if NativeHistorical.mode == 0: ScrollView {",
        1,
    );
    memory=format!("        if root.sub-view == 3: VerticalLayout {{ horizontal-stretch:1;spacing:0;\n            NativeMemoryTabs {{}}\n{}\n            if NativeHistorical.mode != 0: ScrollView {{horizontal-stretch:1;NativeHistoricalControls {{}}}}\n        }}\n",memory);
    source.replace_range(start..end, &memory);
    source
}

pub fn apply_transcribe(mut source: String) -> String {
    source = format!("import {{ NativePlatform }} from \"native-platform.slint\";\n{source}");
    replace(
        &mut source,
        "text: \"File…\";\n                    enabled: !root.busy;",
        "text: \"File…\";\n                    enabled: NativePlatform.pickers && !root.busy;",
        1,
    );
    replace(
        &mut source,
        "enabled: root.output != \"\";",
        "enabled: NativePlatform.clipboard && root.output != \"\";",
        1,
    );
    replace(
        &mut source,
        "enabled: root.output != \"\" && !root.busy;",
        "enabled: NativePlatform.pickers && root.output != \"\" && !root.busy;",
        1,
    );

    source=format!("import {{ NativeTranscription, NativeTranscriptionSetup }} from \"native-transcription.slint\";\n{}",source);
    replace(&mut source,"            // source row: one box for URL or file path","            NativeTranscriptionSetup {}\n            // source row: explicit selected file",1);
    source=source.replace("Paste a YouTube/podcast link or pick an audio/video file. Transcription runs locally; summaries use your selected model.","Choose local audio up to 60 seconds. Canonical speech WAV supports 2 MiB; explicit WAV/AIFF/M4A normalization supports 8 MiB. Review the local executable and model below.");
    source = source.replace(
        "Transcribe audio or a URL, then Save to Artifacts to keep it here.",
        "Transcribe a selected audio file, then Save to Artifacts to keep it here.",
    );
    source = source.replace(
        "https://youtu.be/…  or a file path",
        "Choose an existing local audio file",
    );
    source = source.replace(
        "enabled: !root.busy && root.source != \"\";",
        "enabled: !root.busy && root.source != \"\" && NativeTranscription.can-start;",
    );
    source = source.replace(
        "root.active ? Palette.accent",
        "root.active ? Palette.button-primary",
    );
    source
}

pub fn apply_extensions(mut source: String) -> String {
    replace(&mut source, "Connect an external tool server. Use a direct executable (node / python / uvx) — Windows .cmd shims like npx don't work over stdio.", "External tool server setup is not available in this build.", 1);
    source=format!("import {{ NativeExtensions, NativeExtensionsToolbar, NativePluginExtensions }} from \"native-extensions.slint\";\n{source}");
    let start = source
        .find("component ExtTab inherits Rectangle {")
        .unwrap();
    let end = source[start..]
        .find("export component AgentsAutonomyView")
        .unwrap()
        + start;
    source.replace_range(start..end,r#"component ExtTab inherits SmithButton {
        in property <string> label;in property <string> token;in property <bool> active;callback chosen(string);
        text:root.label;kind:root.active?"primary":"secondary";height:32px;
        width:root.label=="Methods"?100px:90px;clicked=>{root.chosen(root.token);}
    }

"#);
    replace(&mut source,"                        ExtTab { label: \"Tools\"; token: \"tools\"; active: root.ext-tab == \"tools\"; chosen(t) => { root.ext-tab = t; } }", "                        ExtTab { label: \"Tools\"; token: \"tools\"; active: root.ext-tab == \"tools\"; chosen(t) => { root.ext-tab = t; } }\n                        ExtTab { label: \"Plugins\"; token: \"plugins\"; active: root.ext-tab == \"plugins\"; chosen(t) => { root.ext-tab = t; } }",1);
    replace(&mut source,"                // ----- Skills -----", "                NativeExtensionsToolbar {}\n                if root.ext-tab == \"plugins\": NativePluginExtensions {}\n                // ----- Skills -----",1);
    for (kind, var) in [("skill", "sk"), ("method", "mt"), ("recipe", "rc")] {
        let title = match kind {
            "skill" => "skill",
            "method" => "method",
            _ => "tool",
        };
        let old=format!("ConfirmButton {{ label: \"Remove\"; confirm-text: \"Remove {title}?\"; reset-key: {var}.id; enabled: !root.{kind}-adding; confirmed => {{ root.remove-{kind}({var}.id); }} }}");
        let new=format!("HorizontalLayout {{spacing:8px;SmithButton{{text:\"Edit\";enabled:!root.{kind}-adding&&!NativeExtensions.show-trash;clicked=>{{NativeExtensions.edit({var}.id);}}}}if NativeExtensions.show-trash:SmithButton{{text:\"Permanent…\";enabled:!root.{kind}-adding;clicked=>{{NativeExtensions.edit({var}.id);}}}}SmithButton{{text:NativeExtensions.show-trash?\"Restore\":\"Trash\";enabled:!root.{kind}-adding;clicked=>{{root.remove-{kind}({var}.id);}}}}}}");
        replace(&mut source, &old, &new, 1);
        replace(
            &mut source,
            &format!("enabled: !root.{kind}-adding; toggled"),
            &format!("enabled: !root.{kind}-adding && !NativeExtensions.show-trash; toggled"),
            1,
        );
        replace(&mut source,&format!("title: \"Add {title}{}\";",if kind=="recipe"{" (plugin recipe)"}else{""}),&format!("title: NativeExtensions.editing-kind==\"{kind}\"?\"Edit {title}\":\"Add {title}\";"),1);
        let add = if kind == "recipe" {
            "Add tool"
        } else if kind == "skill" {
            "Add skill"
        } else {
            "Add method"
        };
        replace(&mut source,&format!("text: root.{kind}-adding ? \"Working…\" : \"{add}\";"),&format!("text: root.{kind}-adding ? \"Working…\" : NativeExtensions.editing-kind==\"{kind}\"?\"Save {title}\":\"{add}\";"),1);
        replace(
            &mut source,
            &format!("clicked => {{ root.add-{kind}(); }}"),
            &format!("clicked => {{ root.add-{kind}(); }}"),
            1,
        );
        // Keep the source forms, with an explicit new-definition action after editing.
        replace(&mut source,&format!("title: NativeExtensions.editing-kind==\"{kind}\"?\"Edit {title}\":\"Add {title}\";"),&format!("title: NativeExtensions.editing-kind==\"{kind}\"?\"Edit {title}\":\"Add {title}\";\n                        SmithButton {{text:\"New {title}\";enabled:!NativeExtensions.busy;clicked=>{{NativeExtensions.new(\"{kind}\");}}}}"),1);
    }
    source = source.replace(
        "A reusable instruction set. Enabled skills are injected into every agent run.",
        "A reusable instruction set. Enabled skills are captured on later tasks you create.",
    );
    source = source.replace(
        "A learned, reusable workflow. Enabled methods are injected into every agent run.",
        "A reusable workflow. Enabled methods are captured on later tasks you create.",
    );
    source=source.replace("Define a tool as a command template with {placeholders}. The model fills them; the command runs in the WSL2 sandbox.","Retain a command template and parameter schema. Recipe execution is unavailable in this build; saving or enabling never runs it.");
    source=source.replace("JSON command templates the model can call as tools, run in the sandbox. Add one above.","Retained command templates. Execution remains unavailable; records can be edited, trashed and restored.");
    source
}

pub fn apply_models(mut source: String) -> String {
    source = format!(
        "import {{ NativeCatalog, NativeCatalogPane }} from \"native-catalog.slint\";\n{source}"
    );
    replace(
        &mut source,
        "Installed models and recommendations for this PC",
        "Saved local selections and configured connection models",
        1,
    );
    replace(&mut source,"clicked => { root.refresh(); }","clicked => { if NativeCatalog.connection-models { root.refresh(); } else { NativeCatalog.refresh(); } }",1);
    replace(
        &mut source,
        "        if root.default-model-missing: Banner {",
        r#"        HorizontalLayout { spacing:8px;
            SmithButton { text: "Saved local"; kind: NativeCatalog.connection-models ? "secondary" : "primary"; clicked=>{ NativeCatalog.connection-models=false; } }
            SmithButton { text: "Connection models"; kind: NativeCatalog.connection-models ? "primary" : "secondary"; clicked=>{ NativeCatalog.connection-models=true; } }
            Rectangle {horizontal-stretch:1;}
        }
        if !NativeCatalog.connection-models: NativeCatalogPane {}
        if NativeCatalog.connection-models: VerticalLayout { spacing:14px; vertical-stretch:1;
        if root.default-model-missing: Banner {"#,
        1,
    );
    // Close only the introduced provider-list wrapper, retaining the original
    // screen padding, title, row geometry and existing model selection callback.
    let end = source
        .rfind("    }\n}")
        .expect("Original ModelsView ending changed");
    source.insert_str(end, "        }\n");
    for (from,to) in [
      ("Your default model (","Your configured model ("),
      (") is unavailable. Download it again or choose an installed model below.",") is unavailable. Test its connection in Settings or choose an available model."),
      ("Search installed models and Hugging Face...","Filter connection models…"),
      ("Searching Hugging Face... Local matches appear immediately.","Refreshing the explicitly tested connection…"),
      ("Local matches and downloadable GGUF models from Hugging Face","Models reported by the configured connection"),
      ("Online search unavailable","Connection unavailable"),
      ("Checking the publisher catalog.","Checking the configured connection."),
      ("Retry the search or clear it to browse recommendations.","Test the selected connection in Settings."),
      ("Try a different name, or clear the search to see this PC's recommendations.","Test a connection in Settings, then choose a model. Saved local files are in the other tab."),
      ("if item.downloaded && item.id == root.default-model: Text {","if item.id == root.default-model: Text {"),
      ("if item.downloaded && item.id != root.default-model: SmithButton {","if item.id != root.default-model: SmithButton {"),
      ("if !item.downloaded: SmithButton {","if false: SmithButton {"),
    ] {replace(&mut source,from,to,1);}
    source
}
